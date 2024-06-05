// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use crate::cache::EmitCache;
use crate::cache::FastInsecureHasher;
use crate::cache::ParsedSourceCache;

use deno_ast::SourceMapOption;
use deno_ast::TranspileResult;
use deno_core::error::AnyError;
use deno_core::futures::stream::FuturesUnordered;
use deno_core::futures::FutureExt;
use deno_core::futures::StreamExt;
use deno_core::ModuleCodeBytes;
use deno_core::ModuleSpecifier;
use deno_graph::MediaType;
use deno_graph::Module;
use deno_graph::ModuleGraph;
use std::sync::Arc;

pub struct Emitter {
  emit_cache: EmitCache,
  parsed_source_cache: Arc<ParsedSourceCache>,
  transpile_and_emit_options:
    Arc<(deno_ast::TranspileOptions, deno_ast::EmitOptions)>,
  // cached hash of the transpile and emit options
  transpile_and_emit_options_hash: u64,
}

impl Emitter {
  pub fn new(
    emit_cache: EmitCache,
    parsed_source_cache: Arc<ParsedSourceCache>,
    transpile_options: deno_ast::TranspileOptions,
    emit_options: deno_ast::EmitOptions,
  ) -> Self {
    let transpile_and_emit_options_hash = {
      let mut hasher = FastInsecureHasher::new_without_deno_version();
      hasher.write_hashable(&transpile_options);
      hasher.write_hashable(&emit_options);
      hasher.finish()
    };
    Self {
      emit_cache,
      parsed_source_cache,
      transpile_and_emit_options: Arc::new((transpile_options, emit_options)),
      transpile_and_emit_options_hash,
    }
  }

  pub async fn cache_module_emits(
    &self,
    graph: &ModuleGraph,
  ) -> Result<(), AnyError> {
    let mut futures = FuturesUnordered::new();
    for module in graph.modules() {
      let Module::Js(module) = module else {
        continue;
      };

      let is_emittable = matches!(
        module.media_type,
        MediaType::TypeScript
          | MediaType::Mts
          | MediaType::Cts
          | MediaType::Jsx
          | MediaType::Tsx
      );
      if is_emittable {
        futures.push(
          self
            .emit_parsed_source(
              &module.specifier,
              module.media_type,
              &module.source,
            )
            .boxed_local(),
        );
      }
    }

    while let Some(result) = futures.next().await {
      result?; // surface errors
    }

    Ok(())
  }

  /// Gets a cached emit if the source matches the hash found in the cache.
  pub fn maybe_cached_emit(
    &self,
    specifier: &ModuleSpecifier,
    source: &str,
  ) -> Option<Vec<u8>> {
    let source_hash = self.get_source_hash(source);
    self.emit_cache.get_emit_code(specifier, source_hash)
  }

  pub async fn emit_parsed_source(
    &self,
    specifier: &ModuleSpecifier,
    media_type: MediaType,
    source: &Arc<str>,
  ) -> Result<ModuleCodeBytes, AnyError> {
    // Note: keep this in sync with the sync version below
    let helper = EmitParsedSourceHelper(self);
    match helper.pre_emit_parsed_source(specifier, source) {
      PreEmitResult::Cached(emitted_text) => Ok(emitted_text),
      PreEmitResult::NotCached { source_hash } => {
        let parsed_source_cache = self.parsed_source_cache.clone();
        let transpile_and_emit_options =
          self.transpile_and_emit_options.clone();
        let transpile_result = deno_core::unsync::spawn_blocking({
          let specifier = specifier.clone();
          let source = source.clone();
          move || -> Result<_, AnyError> {
            EmitParsedSourceHelper::transpile(
              &parsed_source_cache,
              &specifier,
              source.clone(),
              media_type,
              &transpile_and_emit_options.0,
              &transpile_and_emit_options.1,
            )
          }
        })
        .await
        .unwrap()?;
        Ok(helper.post_emit_parsed_source(
          specifier,
          transpile_result,
          source_hash,
        ))
      }
    }
  }

  pub fn emit_parsed_source_sync(
    &self,
    specifier: &ModuleSpecifier,
    media_type: MediaType,
    source: &Arc<str>,
  ) -> Result<ModuleCodeBytes, AnyError> {
    // Note: keep this in sync with the async version above
    let helper = EmitParsedSourceHelper(self);
    match helper.pre_emit_parsed_source(specifier, source) {
      PreEmitResult::Cached(emitted_text) => Ok(emitted_text),
      PreEmitResult::NotCached { source_hash } => {
        let transpile_result = EmitParsedSourceHelper::transpile(
          &self.parsed_source_cache,
          specifier,
          source.clone(),
          media_type,
          &self.transpile_and_emit_options.0,
          &self.transpile_and_emit_options.1,
        )?;
        Ok(helper.post_emit_parsed_source(
          specifier,
          transpile_result,
          source_hash,
        ))
      }
    }
  }

  /// Expects a file URL, panics otherwise.
  pub async fn load_and_emit_for_hmr(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<String, AnyError> {
    let media_type = MediaType::from_specifier(specifier);
    let source_code = tokio::fs::read_to_string(
      ModuleSpecifier::to_file_path(specifier).unwrap(),
    )
    .await?;
    match media_type {
      MediaType::TypeScript
      | MediaType::Mts
      | MediaType::Cts
      | MediaType::Jsx
      | MediaType::Tsx => {
        let source_arc: Arc<str> = source_code.into();
        let parsed_source = self
          .parsed_source_cache
          .remove_or_parse_module(specifier, source_arc, media_type)?;
        // HMR doesn't work with embedded source maps for some reason, so set
        // the option to not use them (though you should test this out because
        // this statement is probably wrong)
        let mut options = self.transpile_and_emit_options.1.clone();
        options.source_map = SourceMapOption::None;
        let transpiled_source = parsed_source
          .transpile(&self.transpile_and_emit_options.0, &options)?
          .into_source()
          .into_string()?;
        Ok(transpiled_source.text)
      }
      MediaType::JavaScript
      | MediaType::Mjs
      | MediaType::Cjs
      | MediaType::Dts
      | MediaType::Dmts
      | MediaType::Dcts
      | MediaType::Json
      | MediaType::Wasm
      | MediaType::TsBuildInfo
      | MediaType::SourceMap
      | MediaType::Unknown => {
        // clear this specifier from the parsed source cache as it's now out of date
        self.parsed_source_cache.free(specifier);
        Ok(source_code)
      }
    }
  }

  /// A hashing function that takes the source code and uses the global emit
  /// options then generates a string hash which can be stored to
  /// determine if the cached emit is valid or not.
  fn get_source_hash(&self, source_text: &str) -> u64 {
    FastInsecureHasher::new_without_deno_version() // stored in the transpile_and_emit_options_hash
      .write_str(source_text)
      .write_u64(self.transpile_and_emit_options_hash)
      .finish()
  }
}

enum PreEmitResult {
  Cached(ModuleCodeBytes),
  NotCached { source_hash: u64 },
}

/// Helper to share code between async and sync emit_parsed_source methods.
struct EmitParsedSourceHelper<'a>(&'a Emitter);

impl<'a> EmitParsedSourceHelper<'a> {
  pub fn pre_emit_parsed_source(
    &self,
    specifier: &ModuleSpecifier,
    source: &Arc<str>,
  ) -> PreEmitResult {
    let source_hash = self.0.get_source_hash(source);

    if let Some(emit_code) =
      self.0.emit_cache.get_emit_code(specifier, source_hash)
    {
      PreEmitResult::Cached(emit_code.into_boxed_slice().into())
    } else {
      PreEmitResult::NotCached { source_hash }
    }
  }

  pub fn transpile(
    parsed_source_cache: &ParsedSourceCache,
    specifier: &ModuleSpecifier,
    source: Arc<str>,
    media_type: MediaType,
    transpile_options: &deno_ast::TranspileOptions,
    emit_options: &deno_ast::EmitOptions,
  ) -> Result<TranspileResult, AnyError> {
    // nothing else needs the parsed source at this point, so remove from
    // the cache in order to not transpile owned
    let parsed_source = parsed_source_cache
      .remove_or_parse_module(specifier, source, media_type)?;
    Ok(parsed_source.transpile(transpile_options, emit_options)?)
  }

  pub fn post_emit_parsed_source(
    &self,
    specifier: &ModuleSpecifier,
    transpile_result: TranspileResult,
    source_hash: u64,
  ) -> ModuleCodeBytes {
    let transpiled_source = match transpile_result {
      TranspileResult::Owned(source) => source,
      TranspileResult::Cloned(source) => {
        debug_assert!(false, "Transpile owned failed.");
        source
      }
    };
    debug_assert!(transpiled_source.source_map.is_none());
    self.0.emit_cache.set_emit_code(
      specifier,
      source_hash,
      &transpiled_source.source,
    );
    transpiled_source.source.into_boxed_slice().into()
  }
}
