mod chunks;
mod code_generation_results;
mod dependencies;
pub mod entries;

use std::{cell::RefCell, collections::HashMap, path::Path, ptr::NonNull};

use chunks::Chunks;
pub use code_generation_results::*;
use dependencies::JsDependencies;
use entries::JsEntries;
use napi::{NapiRaw, NapiValue};
use napi_derive::napi;
use once_cell::sync::OnceCell;
use rspack_collections::{DatabaseItem, IdentifierSet};
use rspack_core::{
  rspack_sources::BoxSource, BindingCell, BoxDependency, Compilation, CompilationId, EntryOptions,
  FactorizeInfo, ModuleIdentifier, Reflector,
};
use rspack_error::{Diagnostic, ToStringResultToRspackResultExt};
use rspack_napi::{napi::bindgen_prelude::*, OneShotRef, WeakRef};
use rspack_plugin_runtime::RuntimeModuleFromJs;
use rustc_hash::FxHashMap;

use super::PathWithInfo;
use crate::{
  entry::JsEntryOptions, utils::callbackify, AssetInfo, EntryDependency, JsAddingRuntimeModule,
  JsAsset, JsChunk, JsChunkGraph, JsChunkGroupWrapper, JsChunkWrapper, JsCompatSource, JsFilename,
  JsModuleGraph, JsPathData, JsRspackDiagnostic, JsRspackError, JsStats,
  JsStatsOptimizationBailout, ModuleObject, RspackResultToNapiResultExt, ToJsCompatSource,
  COMPILER_REFERENCES,
};

thread_local! {
  static CHUNKS_SYMBOL: OnceCell<OneShotRef> = const { OnceCell::new() };
}

#[napi]
pub struct JsCompilation {
  #[allow(dead_code)]
  pub(crate) id: CompilationId,
  pub(crate) inner: NonNull<Compilation>,
  chunks_ref: OnceCell<WeakRef>,
}

impl JsCompilation {
  fn as_ref(&self) -> napi::Result<&'static Compilation> {
    // SAFETY: The memory address of rspack_core::Compilation will not change,
    // so as long as the Compiler is not dropped, we can safely return a 'static reference.
    Ok(unsafe { self.inner.as_ref() })
  }

  fn as_mut(&mut self) -> napi::Result<&'static mut Compilation> {
    // SAFETY: The memory address of rspack_core::Compilation will not change,
    // so as long as the Compiler is not dropped, we can safely return a 'static reference.
    Ok(unsafe { self.inner.as_mut() })
  }
}

#[napi]
impl JsCompilation {
  #[napi(
    ts_args_type = r#"filename: string, newSourceOrFunction: JsCompatSource | ((source: JsCompatSourceOwned) => JsCompatSourceOwned), assetInfoUpdateOrFunction?: AssetInfo | ((assetInfo: AssetInfo) => AssetInfo | undefined)"#
  )]
  pub fn update_asset(
    &mut self,
    env: &Env,
    filename: String,
    new_source_or_function: Either<JsCompatSource, Function<'_, JsCompatSource, JsCompatSource>>,
    asset_info_update_or_function: Option<Either<Object, Function<'_, Reflector, Option<Object>>>>,
  ) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .update_asset(&filename, |original_source, mut original_info| {
        let new_source: napi::Result<BoxSource> = try {
          let new_source = match new_source_or_function {
            Either::A(new_source) => new_source.into(),
            Either::B(new_source_fn) => {
              let js_compat_source =
                new_source_fn.call(original_source.to_js_compat_source(env)?)?;
              js_compat_source.into()
            }
          };
          new_source
        };
        let new_source = new_source.to_rspack_result()?;

        let new_info: Option<rspack_core::AssetInfo> = match asset_info_update_or_function {
          Some(asset_info_update_or_function) => match asset_info_update_or_function {
            Either::A(object) => {
              let js_asset_info: AssetInfo = unsafe {
                FromNapiValue::from_napi_value(env.raw(), object.raw()).to_rspack_result()?
              };
              Some(js_asset_info.into())
            }
            Either::B(f) => {
              let original_info_object = original_info.reflector();
              let result = f.call(original_info_object).to_rspack_result()?;
              match result {
                Some(object) => {
                  let js_asset_info = AssetInfo::from_jsobject(env, &object).to_rspack_result()?;
                  Some(js_asset_info.into())
                }
                None => None,
              }
            }
          },
          None => None,
        };
        if let Some(new_info) = new_info {
          original_info.merge_another_asset(new_info);
        }
        Ok((new_source, original_info))
      })
      .to_napi_result()
  }

  #[napi(ts_return_type = "Readonly<JsAsset>[]")]
  pub fn get_assets(&self) -> Result<Vec<JsAsset>> {
    let compilation = self.as_ref()?;

    let mut assets = Vec::<JsAsset>::with_capacity(compilation.assets().len());

    for (filename, asset) in compilation.assets() {
      let info = asset.info.reflector();
      assets.push(JsAsset {
        name: filename.clone(),
        info,
      });
    }

    Ok(assets)
  }

  #[napi]
  pub fn get_asset(&self, name: String) -> Result<Option<JsAsset>> {
    let compilation = self.as_ref()?;

    match compilation.assets().get(&name) {
      Some(asset) => {
        let info = asset.info.reflector();
        Ok(Some(JsAsset { name, info }))
      }
      None => Ok(None),
    }
  }

  #[napi]
  pub fn get_asset_source<'a>(
    &self,
    env: &'a Env,
    name: String,
  ) -> Result<Option<JsCompatSource<'a>>> {
    let compilation = self.as_ref()?;

    compilation
      .assets()
      .get(&name)
      .and_then(|v| v.source.as_ref().map(|s| s.to_js_compat_source(env)))
      .transpose()
  }

  #[napi(getter, ts_return_type = "Array<Module>")]
  pub fn modules(&self) -> Result<Vec<ModuleObject>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .get_module_graph()
        .modules()
        .keys()
        .filter_map(|module_id| {
          compilation
            .module_by_identifier(module_id)
            .map(|module| ModuleObject::with_ref(module.as_ref(), compilation.compiler_id()))
        })
        .collect::<Vec<_>>(),
    )
  }

  #[napi(getter, ts_return_type = "Array<Module>")]
  pub fn built_modules(&self) -> Result<Vec<ModuleObject>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .built_modules()
        .iter()
        .filter_map(|module_id| {
          compilation
            .module_by_identifier(module_id)
            .map(|module| ModuleObject::with_ref(module.as_ref(), compilation.compiler_id()))
        })
        .collect::<Vec<_>>(),
    )
  }

  #[napi]
  pub fn get_optimization_bailout(&self) -> Result<Vec<JsStatsOptimizationBailout>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .get_module_graph()
        .module_graph_modules()
        .values()
        .flat_map(|item| item.optimization_bailout.clone())
        .map(|item| JsStatsOptimizationBailout { inner: item })
        .collect::<Vec<_>>(),
    )
  }

  #[napi(getter, ts_return_type = "Chunks")]
  pub fn chunks(
    &self,
    env: &Env,
    mut this: This,
    reference: Reference<JsCompilation>,
  ) -> Result<&WeakRef> {
    let raw_env = env.raw();

    self.chunks_ref.get_or_try_init(move || {
      let symbol: napi::Result<napi::JsObject> = CHUNKS_SYMBOL.with(move |once_cell| {
        let r = once_cell.get_or_try_init(move || {
          let symbol = env.create_symbol(Some("chunks"))?;
          OneShotRef::new(raw_env, symbol)
        })?;

        let napi_val = unsafe { ToNapiValue::to_napi_value(raw_env, r)? };
        Ok(unsafe { Object::from_raw_unchecked(raw_env, napi_val) })
      });

      let mut jsobject = Chunks::new(reference.downgrade()).get_jsobject(env)?;
      this.object.set_property(symbol?, &jsobject)?;
      WeakRef::new(env.raw(), &mut jsobject)
    })
  }

  #[napi]
  pub fn get_named_chunk_keys(&self) -> Result<Vec<String>> {
    let compilation = self.as_ref()?;

    Ok(compilation.named_chunks.keys().cloned().collect::<Vec<_>>())
  }

  #[napi(ts_return_type = "JsChunk | null")]
  pub fn get_named_chunk(&self, name: String) -> Result<Option<JsChunkWrapper>> {
    let compilation = self.as_ref()?;

    Ok(compilation.named_chunks.get(&name).and_then(|c| {
      compilation
        .chunk_by_ukey
        .get(c)
        .map(|chunk| JsChunkWrapper::new(chunk.ukey(), compilation))
    }))
  }

  #[napi]
  pub fn get_named_chunk_group_keys(&self) -> Result<Vec<String>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .named_chunk_groups
        .keys()
        .cloned()
        .collect::<Vec<_>>(),
    )
  }

  #[napi(ts_return_type = "JsChunkGroup")]
  pub fn get_named_chunk_group(&self, name: String) -> Result<Option<JsChunkGroupWrapper>> {
    let compilation = self.as_ref()?;
    Ok(
      compilation
        .named_chunk_groups
        .get(&name)
        .map(|ukey| JsChunkGroupWrapper::new(*ukey, compilation)),
    )
  }

  #[napi]
  pub fn set_asset_source(&mut self, name: String, source: JsCompatSource) -> Result<()> {
    let compilation = self.as_mut()?;

    let source: BoxSource = source.into();
    match compilation.assets_mut().entry(name) {
      std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().set_source(Some(source)),
      std::collections::hash_map::Entry::Vacant(e) => {
        e.insert(rspack_core::CompilationAsset::from(source));
      }
    };
    Ok(())
  }

  #[napi]
  pub fn delete_asset_source(&mut self, name: String) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .assets_mut()
      .entry(name)
      .and_modify(|a| a.set_source(None));
    Ok(())
  }

  #[napi]
  pub fn get_asset_filenames(&self) -> Result<Vec<String>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .assets()
        .iter()
        .filter(|(_, asset)| asset.get_source().is_some())
        .map(|(filename, _)| filename)
        .cloned()
        .collect(),
    )
  }

  #[napi]
  pub fn has_asset(&self, name: String) -> Result<bool> {
    let compilation = self.as_ref()?;

    Ok(compilation.assets().contains_key(&name))
  }

  #[napi(
    ts_args_type = "filename: string, source: JsCompatSource, assetInfo?: AssetInfo | undefined | null"
  )]
  pub fn emit_asset(
    &mut self,
    env: &Env,
    filename: String,
    source: JsCompatSource,
    object: Option<Object>,
  ) -> Result<()> {
    let compilation = self.as_mut()?;

    let asset_info = if let Some(object) = object {
      let js_asset_info = AssetInfo::from_jsobject(env, &object)?;
      let asset_info: rspack_core::AssetInfo = js_asset_info.into();
      let asset_info = BindingCell::from(asset_info);
      asset_info.reflector().set_jsobject(env, object)?;

      asset_info
    } else {
      BindingCell::from(rspack_core::AssetInfo::default())
    };

    compilation.emit_asset(
      filename,
      rspack_core::CompilationAsset {
        source: Some(source.into()),
        info: asset_info,
      },
    );
    Ok(())
  }

  #[napi]
  pub fn delete_asset(&mut self, filename: String) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation.delete_asset(&filename);
    Ok(())
  }

  #[napi]
  pub fn rename_asset(&mut self, filename: String, new_name: String) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation.rename_asset(&filename, new_name);
    Ok(())
  }

  #[napi(getter, ts_return_type = "JsChunkGroup[]")]
  pub fn entrypoints(&self) -> Result<Vec<JsChunkGroupWrapper>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .entrypoints()
        .values()
        .map(|ukey| JsChunkGroupWrapper::new(*ukey, compilation))
        .collect(),
    )
  }

  #[napi(getter, ts_return_type = "JsChunkGroup[]")]
  pub fn chunk_groups(&self) -> Result<Vec<JsChunkGroupWrapper>> {
    let compilation = self.as_ref()?;

    Ok(
      compilation
        .chunk_group_by_ukey
        .keys()
        .map(|ukey| JsChunkGroupWrapper::new(*ukey, compilation))
        .collect::<Vec<JsChunkGroupWrapper>>(),
    )
  }

  #[napi(getter)]
  pub fn hash(&self) -> Result<Option<String>> {
    let compilation = self.as_ref()?;

    Ok(compilation.get_hash().map(|hash| hash.to_owned()))
  }

  #[napi]
  pub fn dependencies(&'static self) -> Result<JsDependencies> {
    let compilation = self.as_ref()?;

    Ok(JsDependencies::new(compilation))
  }

  #[napi]
  pub fn push_diagnostic(&mut self, diagnostic: JsRspackDiagnostic) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation.push_diagnostic(diagnostic.into());
    Ok(())
  }

  #[napi]
  pub fn splice_diagnostic(
    &mut self,
    start: u32,
    end: u32,
    replace_with: Vec<crate::JsRspackDiagnostic>,
  ) -> Result<()> {
    let compilation = self.as_mut()?;

    let diagnostics = replace_with.into_iter().map(Into::into).collect();
    compilation.splice_diagnostic(start as usize, end as usize, diagnostics);
    Ok(())
  }

  #[napi(ts_args_type = r#"diagnostic: ExternalObject<'Diagnostic'>"#)]
  pub fn push_native_diagnostic(&mut self, diagnostic: &External<Diagnostic>) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation.push_diagnostic((**diagnostic).clone());
    Ok(())
  }

  #[napi(ts_args_type = r#"diagnostics: ExternalObject<'Diagnostic[]'>"#)]
  pub fn push_native_diagnostics(
    &mut self,
    diagnostics: &mut External<Vec<Diagnostic>>,
  ) -> Result<()> {
    let compilation = self.as_mut()?;

    while let Some(diagnostic) = diagnostics.pop() {
      compilation.push_diagnostic(diagnostic);
    }
    Ok(())
  }

  #[napi]
  pub fn get_errors(&self) -> Result<Vec<JsRspackError>> {
    let compilation = self.as_ref()?;

    let colored = compilation.options.stats.colors;
    Ok(
      compilation
        .get_errors_sorted()
        .map(|d| {
          JsRspackError::try_from_diagnostic(d, colored)
            .expect("should convert diagnostic to `JsRspackError`")
        })
        .collect(),
    )
  }

  #[napi]
  pub fn get_warnings(&self) -> Result<Vec<JsRspackError>> {
    let compilation = self.as_ref()?;

    let colored = compilation.options.stats.colors;
    Ok(
      compilation
        .get_warnings_sorted()
        .map(|d| {
          JsRspackError::try_from_diagnostic(d, colored)
            .expect("should convert diagnostic to `JsRspackError`")
        })
        .collect(),
    )
  }

  #[napi]
  pub fn get_stats(&self, reference: Reference<JsCompilation>, env: Env) -> Result<JsStats> {
    Ok(JsStats::new(reference.share_with(env, |compilation| {
      let compilation = compilation.as_ref()?;

      Ok(compilation.get_stats())
    })?))
  }

  #[napi]
  pub fn get_asset_path(&self, filename: String, data: JsPathData) -> Result<String> {
    let compilation = self.as_ref()?;
    #[allow(clippy::disallowed_methods)]
    futures::executor::block_on(compilation.get_asset_path(&filename.into(), data.to_path_data()))
      .to_napi_result()
  }

  #[napi]
  pub fn get_asset_path_with_info(
    &self,
    filename: String,
    data: JsPathData,
  ) -> Result<PathWithInfo> {
    let compilation = self.as_ref()?;

    #[allow(clippy::disallowed_methods)]
    let res = futures::executor::block_on(
      compilation.get_asset_path_with_info(&filename.into(), data.to_path_data()),
    )
    .to_napi_result()?;
    Ok(res.into())
  }

  #[napi]
  pub fn get_path(&self, filename: String, data: JsPathData) -> Result<String> {
    let compilation = self.as_ref()?;
    #[allow(clippy::disallowed_methods)]
    futures::executor::block_on(compilation.get_path(&filename.into(), data.to_path_data()))
      .to_napi_result()
  }

  #[napi]
  pub fn get_path_with_info(&self, filename: String, data: JsPathData) -> Result<PathWithInfo> {
    let compilation = self.as_ref()?;

    let mut asset_info = rspack_core::AssetInfo::default();

    #[allow(clippy::disallowed_methods)]
    let path = futures::executor::block_on(compilation.get_path_with_info(
      &filename.into(),
      data.to_path_data(),
      &mut asset_info,
    ))
    .to_napi_result()?;
    Ok((path, asset_info).into())
  }

  #[napi]
  pub fn add_file_dependencies(&mut self, deps: Vec<String>) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .file_dependencies
      .extend(deps.into_iter().map(|s| Path::new(&s).into()));
    Ok(())
  }

  #[napi]
  pub fn add_context_dependencies(&mut self, deps: Vec<String>) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .context_dependencies
      .extend(deps.into_iter().map(|s| Path::new(&s).into()));
    Ok(())
  }

  #[napi]
  pub fn add_missing_dependencies(&mut self, deps: Vec<String>) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .missing_dependencies
      .extend(deps.into_iter().map(|s| Path::new(&s).into()));
    Ok(())
  }

  #[napi]
  pub fn add_build_dependencies(&mut self, deps: Vec<String>) -> Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .build_dependencies
      .extend(deps.into_iter().map(|s| Path::new(&s).into()));
    Ok(())
  }

  /// This is a very unsafe function.
  /// Please don't use this at the moment.
  /// Using async and mutable reference to `Compilation` at the same time would likely to cause data races.
  #[napi]
  pub fn rebuild_module(
    &mut self,
    reference: Reference<JsCompilation>,
    module_identifiers: Vec<String>,
    f: Function,
  ) -> Result<()> {
    let compilation = self.as_mut()?;

    callbackify(
      f,
      async {
        let compiler_id = compilation.compiler_id();

        let modules = compilation
          .rebuild_module(
            IdentifierSet::from_iter(module_identifiers.into_iter().map(ModuleIdentifier::from)),
            |modules| {
              modules
                .into_iter()
                .map(|module| ModuleObject::with_ref(module.as_ref(), compiler_id))
                .collect::<Vec<_>>()
            },
          )
          .await
          .to_napi_result()?;

        Ok(modules)
      },
      || drop(reference),
    )
  }

  #[allow(clippy::too_many_arguments)]
  #[napi]
  pub fn import_module(
    &self,
    reference: Reference<JsCompilation>,
    request: String,
    layer: Option<String>,
    public_path: Option<JsFilename>,
    base_uri: Option<String>,
    original_module: Option<String>,
    original_module_context: Option<String>,
    callback: Function,
  ) -> Result<()> {
    let compilation = self.as_ref()?;

    callbackify(
      callback,
      async {
        let module_executor = compilation
          .module_executor
          .as_ref()
          .expect("should have module executor");
        let res = module_executor
          .import_module(
            request,
            layer,
            public_path.map(|p| p.into()),
            base_uri,
            original_module_context.map(rspack_core::Context::from),
            original_module.map(ModuleIdentifier::from),
          )
          .await;

        let js_result = JsExecuteModuleResult {
          cacheable: res.cacheable,
          file_dependencies: res
            .file_dependencies
            .into_iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect(),
          context_dependencies: res
            .context_dependencies
            .into_iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect(),
          build_dependencies: res
            .build_dependencies
            .into_iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect(),
          missing_dependencies: res
            .missing_dependencies
            .into_iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect(),
          id: res.id,
          error: res.error,
        };
        Ok(js_result)
      },
      || {
        drop(reference);
      },
    )
  }

  #[napi(getter)]
  pub fn entries(&mut self) -> Result<JsEntries> {
    let compilation = self.as_mut()?;

    Ok(JsEntries::new(compilation))
  }

  #[napi]
  pub fn add_runtime_module(
    &mut self,
    chunk: &JsChunk,
    runtime_module: JsAddingRuntimeModule,
  ) -> napi::Result<()> {
    let compilation = self.as_mut()?;

    compilation
      .add_runtime_module(
        &chunk.chunk_ukey,
        Box::new(RuntimeModuleFromJs::from(runtime_module)),
      )
      .to_napi_result()
  }

  #[napi(getter)]
  pub fn module_graph(&self) -> napi::Result<JsModuleGraph> {
    let compilation = self.as_ref()?;
    Ok(JsModuleGraph::new(compilation))
  }

  #[napi(getter)]
  pub fn chunk_graph(&self) -> napi::Result<JsChunkGraph> {
    let compilation = self.as_ref()?;
    Ok(JsChunkGraph::new(compilation))
  }

  #[napi(
    ts_args_type = "args: [string, EntryDependency, JsEntryOptions | undefined][], callback: (errMsg: Error | null, results: [string | null, Module][]) => void"
  )]
  pub fn add_entry(
    &mut self,
    reference: Reference<JsCompilation>,
    js_args: Vec<(String, &mut EntryDependency, Option<JsEntryOptions>)>,
    f: Function,
  ) -> napi::Result<()> {
    let compilation = self.as_mut()?;

    let Some(mut compiler_reference) = COMPILER_REFERENCES.with(|ref_cell| {
      let references = ref_cell.borrow_mut();
      references.get(&compilation.compiler_id()).cloned()
    }) else {
      return Err(napi::Error::from_reason(
        "Unable to addEntry now. The Compiler has been garbage collected by JavaScript.",
      ));
    };
    let Some(js_compiler) = compiler_reference.get_mut() else {
      return Err(napi::Error::from_reason(
        "Unable to addEntry now. The Compiler has been garbage collected by JavaScript.",
      ));
    };
    let entry_dependencies_map = &mut js_compiler.entry_dependencies_map;

    let args = js_args
      .into_iter()
      .map(|(js_context, js_dependency, js_options)| {
        let layer = match &js_options {
          Some(options) => options.layer.clone(),
          None => None,
        };
        let options = match js_options {
          Some(js_opts) => js_opts.into(),
          None => EntryOptions::default(),
        };
        let dependency = if let Some(map) = entry_dependencies_map.get(&js_dependency.request)
          && let Some(dependency) = map.get(&options)
        {
          js_dependency.dependency_id = Some(*dependency.id());
          dependency.clone()
        } else {
          let dependency = js_dependency.resolve(js_context.into(), layer)?;
          if let Some(map) = entry_dependencies_map.get_mut(&js_dependency.request) {
            map.insert(options.clone(), dependency.clone());
          } else {
            let mut map = FxHashMap::default();
            map.insert(options.clone(), dependency.clone());
            entry_dependencies_map.insert(js_dependency.request.to_string(), map);
          }
          dependency
        };
        Ok((dependency, options))
      })
      .collect::<napi::Result<Vec<(BoxDependency, EntryOptions)>>>()?;

    callbackify(
      f,
      async move {
        let dependency_ids = args
          .iter()
          .map(|(dependency, _)| *dependency.id())
          .collect::<Vec<_>>();

        compilation.add_entry_batch(args).await.to_napi_result()?;

        let module_graph = compilation.get_module_graph();
        let results = dependency_ids
          .into_iter()
          .map(|dependency_id| {
            if let Some(dependency) = module_graph.dependency_by_id(&dependency_id) {
              if let Some(factorize_info) = FactorizeInfo::get_from(dependency) {
                if let Some(diagnostic) = factorize_info.diagnostics().first() {
                  return Either::A(diagnostic.to_string());
                }
              }
            }

            match module_graph.get_module_by_dependency_id(&dependency_id) {
              Some(module) => {
                let js_module = ModuleObject::with_ref(module.as_ref(), compilation.compiler_id());
                Either::B(js_module)
              }
              None => Either::A("build failed with unknown error".to_string()),
            }
          })
          .collect::<Vec<Either<String, ModuleObject>>>();

        Ok(JsAddEntryItemCallbackArgs(results))
      },
      || {
        drop(reference);
      },
    )
  }

  #[napi(
    ts_args_type = "args: [string, EntryDependency, JsEntryOptions | undefined][], callback: (errMsg: Error | null, results: [string | null, Module][]) => void"
  )]
  pub fn add_include(
    &mut self,
    reference: Reference<JsCompilation>,
    js_args: Vec<(String, &mut EntryDependency, Option<JsEntryOptions>)>,
    f: Function,
  ) -> napi::Result<()> {
    let compilation = self.as_mut()?;

    let Some(mut compiler_reference) = COMPILER_REFERENCES.with(|ref_cell| {
      let references = ref_cell.borrow_mut();
      references.get(&compilation.compiler_id()).cloned()
    }) else {
      return Err(napi::Error::from_reason(
        "Unable to addInclude now. The Compiler has been garbage collected by JavaScript.",
      ));
    };
    let Some(js_compiler) = compiler_reference.get_mut() else {
      return Err(napi::Error::from_reason(
        "Unable to addInclude now. The Compiler has been garbage collected by JavaScript.",
      ));
    };
    let include_dependencies_map = &mut js_compiler.include_dependencies_map;

    let args = js_args
      .into_iter()
      .map(|(js_context, js_dependency, js_options)| {
        let layer = match &js_options {
          Some(options) => options.layer.clone(),
          None => None,
        };
        let options = match js_options {
          Some(js_opts) => js_opts.into(),
          None => EntryOptions::default(),
        };
        let dependency = if let Some(map) = include_dependencies_map.get(&js_dependency.request)
          && let Some(dependency) = map.get(&options)
        {
          js_dependency.dependency_id = Some(*dependency.id());
          dependency.clone()
        } else {
          let dependency = js_dependency.resolve(js_context.into(), layer)?;
          if let Some(map) = include_dependencies_map.get_mut(&js_dependency.request) {
            map.insert(options.clone(), dependency.clone());
          } else {
            let mut map = FxHashMap::default();
            map.insert(options.clone(), dependency.clone());
            include_dependencies_map.insert(js_dependency.request.to_string(), map);
          }
          dependency
        };
        Ok((dependency, options))
      })
      .collect::<napi::Result<Vec<(BoxDependency, EntryOptions)>>>()?;

    callbackify(
      f,
      async move {
        let dependency_ids = args
          .iter()
          .map(|(dependency, _)| *dependency.id())
          .collect::<Vec<_>>();

        compilation.add_include(args).await.to_napi_result()?;

        let module_graph = compilation.get_module_graph();
        let results = dependency_ids
          .into_iter()
          .map(|dependency_id| {
            if let Some(dependency) = module_graph.dependency_by_id(&dependency_id) {
              if let Some(factorize_info) = FactorizeInfo::get_from(dependency) {
                if let Some(diagnostic) = factorize_info.diagnostics().first() {
                  return Either::A(diagnostic.to_string());
                }
              }
            }

            match module_graph.get_module_by_dependency_id(&dependency_id) {
              Some(module) => {
                let js_module = ModuleObject::with_ref(module.as_ref(), compilation.compiler_id());
                Either::B(js_module)
              }
              None => Either::A("build failed with unknown error".to_string()),
            }
          })
          .collect::<Vec<Either<String, ModuleObject>>>();

        Ok(JsAddEntryItemCallbackArgs(results))
      },
      || {
        drop(reference);
      },
    )
  }

  #[napi(getter, ts_return_type = "CodeGenerationResults")]
  pub fn code_generation_results(&self) -> Result<Reflector> {
    let compilation = self.as_ref()?;

    Ok(compilation.code_generation_results.reflector())
  }
}

pub struct JsAddEntryItemCallbackArgs(Vec<Either<String, ModuleObject>>);

impl ToNapiValue for JsAddEntryItemCallbackArgs {
  unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
    let env_wrapper = Env::from_raw(env);
    let mut js_array = env_wrapper.create_array(0)?;

    for result in val.0 {
      let js_result = match result {
        Either::A(msg) => vec![
          env_wrapper.create_string(&msg)?.into_unknown(),
          env_wrapper.get_undefined()?.into_unknown(),
        ],
        Either::B(module) => {
          let napi_val = ToNapiValue::to_napi_value(env, module)?;
          let js_module = Unknown::from_napi_value(env, napi_val)?;
          vec![env_wrapper.get_undefined()?.into_unknown(), js_module]
        }
      };
      js_array.insert(js_result)?;
    }

    ToNapiValue::to_napi_value(env, js_array)
  }
}

thread_local! {
  // The reason for using WeakReference<JsCompilation> to store the Compilation JS Object handle is that,
  // on the JS side, the Compiler JS Object holds the Compilation through its properties.
  // Therefore, the lifecycle of the Compilation JS Object is tied to the Compiler JS Object.
  //
  // Another point to consider is that when users manually call the build method on Compilation and trigger hooks,
  // Rust no longer maintains the handle mapping, which can cause issues.
  // The solution is to avoid passing Compilation from Rust in the hooks within Compilation and handle it on the JS side instead.
  static COMPILATION_INSTANCE_REFS: RefCell<HashMap<CompilationId, WeakReference<JsCompilation>>> = Default::default();
}

// The difference between JsCompilationWrapper and JsCompilation is:
// JsCompilationWrapper maintains a cache to ensure that the corresponding instance of the same Compilation is unique on the JS side.
//
// This means that when transferring a JsCompilation from Rust to JS, you must use JsCompilationWrapper instead.
pub struct JsCompilationWrapper {
  id: CompilationId,
  inner: NonNull<Compilation>,
}

unsafe impl Send for JsCompilationWrapper {}

impl JsCompilationWrapper {
  pub fn new(compilation: &Compilation) -> Self {
    #[allow(clippy::unwrap_used)]
    Self {
      id: compilation.id(),
      inner: NonNull::new(compilation as *const Compilation as *mut Compilation).unwrap(),
    }
  }

  pub fn cleanup_last_compilation(compilation_id: CompilationId) {
    COMPILATION_INSTANCE_REFS.with(|ref_cell| {
      let mut refs = ref_cell.borrow_mut();
      refs.remove(&compilation_id);
    });
  }
}

impl ToNapiValue for JsCompilationWrapper {
  unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
    COMPILATION_INSTANCE_REFS.with(|ref_cell| {
      let mut refs = ref_cell.borrow_mut();

      match refs.entry(val.id) {
        std::collections::hash_map::Entry::Occupied(entry) => {
          let r = entry.get();
          ToNapiValue::to_napi_value(env, r.clone())
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
          let js_compilation = JsCompilation {
            id: val.id,
            inner: val.inner,
            chunks_ref: OnceCell::new(),
          };
          let napi_value = ToNapiValue::to_napi_value(env, js_compilation)?;
          let reference: Reference<JsCompilation> = Reference::from_napi_value(env, napi_value)?;
          let weak_reference = entry.insert(reference.downgrade());
          ToNapiValue::to_napi_value(env, weak_reference.clone())
        }
      }
    })
  }
}

#[napi(object)]
pub struct JsExecuteModuleResult {
  pub file_dependencies: Vec<String>,
  pub context_dependencies: Vec<String>,
  pub build_dependencies: Vec<String>,
  pub missing_dependencies: Vec<String>,
  pub cacheable: bool,
  pub id: u32,
  pub error: Option<String>,
}

#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsBuildTimeExecutionOption {
  pub public_path: Option<String>,
  pub base_uri: Option<String>,
}
