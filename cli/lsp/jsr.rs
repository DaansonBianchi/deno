// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use crate::args::jsr_api_url;
use crate::args::jsr_url;
use crate::file_fetcher::FileFetcher;
use crate::jsr::partial_jsr_package_version_info_from_slice;
use crate::jsr::JsrFetchResolver;
use dashmap::DashMap;
use deno_cache_dir::HttpCache;
use deno_core::anyhow::anyhow;
use deno_core::error::AnyError;
use deno_core::serde_json;
use deno_graph::packages::JsrPackageInfo;
use deno_graph::packages::JsrPackageInfoVersion;
use deno_graph::packages::JsrPackageVersionInfo;
use deno_graph::ModuleSpecifier;
use deno_runtime::deno_permissions::PermissionsContainer;
use deno_semver::jsr::JsrPackageReqReference;
use deno_semver::package::PackageNv;
use deno_semver::package::PackageReq;
use deno_semver::Version;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use super::config::Config;
use super::config::ConfigData;
use super::search::PackageSearchApi;

/// Keep in sync with `JsrFetchResolver`!
#[derive(Debug)]
pub struct JsrCacheResolver {
  nv_by_req: DashMap<PackageReq, Option<PackageNv>>,
  /// The `module_graph` fields of the version infos should be forcibly absent.
  /// It can be large and we don't want to store it.
  info_by_nv: DashMap<PackageNv, Option<Arc<JsrPackageVersionInfo>>>,
  info_by_name: DashMap<String, Option<Arc<JsrPackageInfo>>>,
  workspace_scope_by_name: HashMap<String, ModuleSpecifier>,
  cache: Arc<dyn HttpCache>,
}

impl JsrCacheResolver {
  pub fn new(
    cache: Arc<dyn HttpCache>,
    config_data: Option<&ConfigData>,
    config: &Config,
  ) -> Self {
    let nv_by_req = DashMap::new();
    let info_by_nv = DashMap::new();
    let info_by_name = DashMap::new();
    let mut workspace_scope_by_name = HashMap::new();
    if let Some(config_data) = config_data {
      let config_data_by_scope = config.tree.data_by_scope();
      for member_scope in config_data.workspace_members.as_ref() {
        let Some(member_data) = config_data_by_scope.get(member_scope) else {
          continue;
        };
        let Some(package_config) = member_data.package_config.as_ref() else {
          continue;
        };
        info_by_name.insert(
          package_config.nv.name.clone(),
          Some(Arc::new(JsrPackageInfo {
            versions: [(
              package_config.nv.version.clone(),
              JsrPackageInfoVersion { yanked: false },
            )]
            .into_iter()
            .collect(),
          })),
        );
        info_by_nv.insert(
          package_config.nv.clone(),
          Some(Arc::new(JsrPackageVersionInfo {
            exports: package_config.exports.clone(),
            module_graph_1: None,
            module_graph_2: None,
            manifest: Default::default(),
          })),
        );
        workspace_scope_by_name
          .insert(package_config.nv.name.clone(), member_scope.clone());
      }
    }
    if let Some(lockfile) = config_data.and_then(|d| d.lockfile.as_ref()) {
      for (req_url, nv_url) in &lockfile.lock().content.packages.specifiers {
        let Some(req) = req_url.strip_prefix("jsr:") else {
          continue;
        };
        let Some(nv) = nv_url.strip_prefix("jsr:") else {
          continue;
        };
        let Ok(req) = PackageReq::from_str(req) else {
          continue;
        };
        let Ok(nv) = PackageNv::from_str(nv) else {
          continue;
        };
        nv_by_req.insert(req, Some(nv));
      }
    }
    Self {
      nv_by_req,
      info_by_nv,
      info_by_name,
      workspace_scope_by_name,
      cache: cache.clone(),
    }
  }

  pub fn req_to_nv(&self, req: &PackageReq) -> Option<PackageNv> {
    if let Some(nv) = self.nv_by_req.get(req) {
      return nv.value().clone();
    }
    let maybe_get_nv = || {
      let name = req.name.clone();
      let package_info = self.package_info(&name)?;
      // Find the first matching version of the package which is cached.
      let mut versions = package_info.versions.keys().collect::<Vec<_>>();
      versions.sort();
      let version = versions
        .into_iter()
        .rev()
        .find(|v| {
          if req.version_req.tag().is_some() || !req.version_req.matches(v) {
            return false;
          }
          let nv = PackageNv {
            name: name.clone(),
            version: (*v).clone(),
          };
          self.package_version_info(&nv).is_some()
        })
        .cloned()?;
      Some(PackageNv { name, version })
    };
    let nv = maybe_get_nv();
    self.nv_by_req.insert(req.clone(), nv.clone());
    nv
  }

  pub fn jsr_to_resource_url(
    &self,
    req_ref: &JsrPackageReqReference,
  ) -> Option<ModuleSpecifier> {
    let req = req_ref.req().clone();
    let maybe_nv = self.req_to_nv(&req);
    let nv = maybe_nv.as_ref()?;
    let info = self.package_version_info(nv)?;
    let path = info.export(&normalize_export_name(req_ref.sub_path()))?;
    if let Some(workspace_scope) = self.workspace_scope_by_name.get(&nv.name) {
      workspace_scope.join(path).ok()
    } else {
      jsr_url()
        .join(&format!("{}/{}/{}", &nv.name, &nv.version, &path))
        .ok()
    }
  }

  pub fn lookup_export_for_path(
    &self,
    nv: &PackageNv,
    path: &str,
  ) -> Option<String> {
    let info = self.package_version_info(nv)?;
    let path = path.strip_prefix("./").unwrap_or(path);
    let mut sloppy_fallback = None;
    for (export, path_) in info.exports() {
      let path_ = path_.strip_prefix("./").unwrap_or(path_);
      if path_ == path {
        return Some(export.strip_prefix("./").unwrap_or(export).to_string());
      }
      // TSC in some cases will suggest a `.js` import path for a `.d.ts` source
      // file.
      if sloppy_fallback.is_none() {
        let path = path
          .strip_suffix(".js")
          .or_else(|| path.strip_suffix(".mjs"))
          .or_else(|| path.strip_suffix(".cjs"))
          .unwrap_or(path);
        let path_ = path_
          .strip_suffix(".d.ts")
          .or_else(|| path_.strip_suffix(".d.mts"))
          .or_else(|| path_.strip_suffix(".d.cts"))
          .unwrap_or(path_);
        if path_ == path {
          sloppy_fallback =
            Some(export.strip_prefix("./").unwrap_or(export).to_string());
        }
      }
    }
    sloppy_fallback
  }

  pub fn lookup_req_for_nv(&self, nv: &PackageNv) -> Option<PackageReq> {
    for entry in self.nv_by_req.iter() {
      let Some(nv_) = entry.value() else {
        continue;
      };
      if nv_ == nv {
        return Some(entry.key().clone());
      }
    }
    None
  }

  pub fn package_info(&self, name: &str) -> Option<Arc<JsrPackageInfo>> {
    if let Some(info) = self.info_by_name.get(name) {
      return info.value().clone();
    }
    let read_cached_package_info = || {
      let meta_url = jsr_url().join(&format!("{}/meta.json", name)).ok()?;
      let meta_bytes = read_cached_url(&meta_url, &self.cache)?;
      serde_json::from_slice::<JsrPackageInfo>(&meta_bytes).ok()
    };
    let info = read_cached_package_info().map(Arc::new);
    self.info_by_name.insert(name.to_string(), info.clone());
    info
  }

  pub fn package_version_info(
    &self,
    nv: &PackageNv,
  ) -> Option<Arc<JsrPackageVersionInfo>> {
    if let Some(info) = self.info_by_nv.get(nv) {
      return info.value().clone();
    }
    let read_cached_package_version_info = || {
      let meta_url = jsr_url()
        .join(&format!("{}/{}_meta.json", &nv.name, &nv.version))
        .ok()?;
      let meta_bytes = read_cached_url(&meta_url, &self.cache)?;
      partial_jsr_package_version_info_from_slice(&meta_bytes).ok()
    };
    let info = read_cached_package_version_info().map(Arc::new);
    self.info_by_nv.insert(nv.clone(), info.clone());
    info
  }

  pub fn did_cache(&self) {
    self.nv_by_req.retain(|_, nv| nv.is_some());
    self.info_by_nv.retain(|_, info| info.is_some());
    self.info_by_name.retain(|_, info| info.is_some());
  }
}

fn read_cached_url(
  url: &ModuleSpecifier,
  cache: &Arc<dyn HttpCache>,
) -> Option<Vec<u8>> {
  cache
    .get(&cache.cache_item_key(url).ok()?, None)
    .ok()?
    .map(|f| f.content)
}

// TODO(nayeemrmn): This is duplicated from a private function in deno_graph
// 0.65.1. Make it public or cleanup otherwise.
fn normalize_export_name(sub_path: Option<&str>) -> Cow<str> {
  let Some(sub_path) = sub_path else {
    return Cow::Borrowed(".");
  };
  if sub_path.is_empty() || matches!(sub_path, "/" | ".") {
    Cow::Borrowed(".")
  } else {
    let sub_path = if sub_path.starts_with('/') {
      Cow::Owned(format!(".{}", sub_path))
    } else if !sub_path.starts_with("./") {
      Cow::Owned(format!("./{}", sub_path))
    } else {
      Cow::Borrowed(sub_path)
    };
    if let Some(prefix) = sub_path.strip_suffix('/') {
      Cow::Owned(prefix.to_string())
    } else {
      sub_path
    }
  }
}

#[derive(Debug)]
pub struct CliJsrSearchApi {
  file_fetcher: Arc<FileFetcher>,
  resolver: JsrFetchResolver,
  search_cache: DashMap<String, Arc<Vec<String>>>,
  versions_cache: DashMap<String, Arc<Vec<Version>>>,
  exports_cache: DashMap<PackageNv, Arc<Vec<String>>>,
}

impl CliJsrSearchApi {
  pub fn new(file_fetcher: Arc<FileFetcher>) -> Self {
    let resolver = JsrFetchResolver::new(file_fetcher.clone());
    Self {
      file_fetcher,
      resolver,
      search_cache: Default::default(),
      versions_cache: Default::default(),
      exports_cache: Default::default(),
    }
  }

  pub fn get_resolver(&self) -> &JsrFetchResolver {
    &self.resolver
  }

  pub fn clear_cache(&self) {
    self.file_fetcher.clear_memory_files();
    self.search_cache.clear();
    self.versions_cache.clear();
    self.exports_cache.clear();
  }
}

#[async_trait::async_trait]
impl PackageSearchApi for CliJsrSearchApi {
  async fn search(&self, query: &str) -> Result<Arc<Vec<String>>, AnyError> {
    if let Some(names) = self.search_cache.get(query) {
      return Ok(names.clone());
    }
    let mut search_url = jsr_api_url().join("packages")?;
    search_url.query_pairs_mut().append_pair("query", query);
    let file_fetcher = self.file_fetcher.clone();
    // spawn due to the lsp's `Send` requirement
    let file = deno_core::unsync::spawn(async move {
      file_fetcher
        .fetch(&search_url, &PermissionsContainer::allow_all())
        .await?
        .into_text_decoded()
    })
    .await??;
    let names = Arc::new(parse_jsr_search_response(&file.source)?);
    self.search_cache.insert(query.to_string(), names.clone());
    Ok(names)
  }

  async fn versions(&self, name: &str) -> Result<Arc<Vec<Version>>, AnyError> {
    if let Some(versions) = self.versions_cache.get(name) {
      return Ok(versions.clone());
    }
    let info = self
      .resolver
      .package_info(name)
      .await
      .ok_or_else(|| anyhow!("JSR package info not found: {}", name))?;
    let mut versions = info.versions.keys().cloned().collect::<Vec<_>>();
    versions.sort();
    versions.reverse();
    let versions = Arc::new(versions);
    self
      .versions_cache
      .insert(name.to_string(), versions.clone());
    Ok(versions)
  }

  async fn exports(
    &self,
    nv: &PackageNv,
  ) -> Result<Arc<Vec<String>>, AnyError> {
    if let Some(exports) = self.exports_cache.get(nv) {
      return Ok(exports.clone());
    }
    let info = self
      .resolver
      .package_version_info(nv)
      .await
      .ok_or_else(|| anyhow!("JSR package version info not found: {}", nv))?;
    let mut exports = info
      .exports()
      .map(|(n, _)| n.to_string())
      .collect::<Vec<_>>();
    exports.sort();
    let exports = Arc::new(exports);
    self.exports_cache.insert(nv.clone(), exports.clone());
    Ok(exports)
  }
}

fn parse_jsr_search_response(source: &str) -> Result<Vec<String>, AnyError> {
  #[derive(Debug, Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct Item {
    scope: String,
    name: String,
    version_count: usize,
  }
  #[derive(Debug, Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct Response {
    items: Vec<Item>,
  }
  let items = serde_json::from_str::<Response>(source)?.items;
  Ok(
    items
      .into_iter()
      .filter(|i| i.version_count > 0)
      .map(|i| format!("@{}/{}", i.scope, i.name))
      .collect(),
  )
}
