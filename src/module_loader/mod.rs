//! Resolves `require` for files on disk: relative paths, `init.nyk`
//! directories and search paths.

pub mod cache;
pub mod packages;
pub mod path;
pub mod searcher;

pub use cache::ModuleCache;
pub use path::{PathEngine, PathResolutionError};
pub use searcher::SearcherEngine;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bytecode::proto::Proto;

/// A module file found by `ModuleLoader::locate_module`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    /// A normalized path inside the running app's bundle.
    Bundled(String),
    Disk(PathBuf),
}

impl Located {
    /// The same key however `require` spelled the module (`app/rail`,
    /// `./app/rail`, `app.rail`, `app/rail.nyk`), so each file runs once
    /// per VM. The prefix keeps it apart from the standard library names.
    pub fn cache_key(&self) -> String {
        match self {
            Located::Bundled(path) => format!("bundle:{}", path),
            Located::Disk(path) => {
                let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                format!("file:{}", canonical.display())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModuleLoader {
    searcher: SearcherEngine,
    cache: ModuleCache,
    base_dir: Option<PathBuf>,
}

impl Default for ModuleLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleLoader {
    pub fn new() -> Self {
        Self {
            searcher: SearcherEngine::new(),
            cache: ModuleCache::new(),
            base_dir: None,
        }
    }

    pub fn with_base_dir(mut self, base_dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }

    pub fn searcher(&self) -> &SearcherEngine {
        &self.searcher
    }

    pub fn searcher_mut(&mut self) -> &mut SearcherEngine {
        &mut self.searcher
    }

    pub fn cache(&self) -> &ModuleCache {
        &self.cache
    }

    pub fn cache_mut(&mut self) -> &mut ModuleCache {
        &mut self.cache
    }

    /// Resolves an external module path request using searchers or relative
    /// paths: against the base directory first, then the working directory.
    pub fn resolve_module(&self, requested: &str) -> Result<PathBuf, String> {
        let base = self.base_dir.as_deref();
        match self.resolve_from(requested, base) {
            Err(_) if base.is_some() => self.resolve_from(requested, None),
            found => found,
        }
    }

    fn resolve_from(&self, requested: &str, base: Option<&Path>) -> Result<PathBuf, String> {
        // 1. Try searcher patterns first (e.g. ./?.nyk, ./modules/?.nyk)
        if let Some(found) = self.searcher.search(requested, base) {
            return Ok(found);
        }

        // 2. Try direct relative path resolution (e.g. ./sub/utils or utils/math)
        match PathEngine::resolve_candidate(requested, base) {
            Ok(found) => Ok(found),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Finds a module file: in the running app's bundle when it has the
    /// module, otherwise on disk.
    pub fn locate_module(&self, requested: &str) -> Result<Located, String> {
        if let Some(path) = self.find_bundled_module(requested) {
            return Ok(Located::Bundled(path));
        }
        self.resolve_module(requested).map(Located::Disk)
    }

    /// Reads a module file found by `locate_module`.
    pub fn read_located(&self, located: &Located) -> Result<(PathBuf, Vec<u8>), String> {
        match located {
            Located::Bundled(path) => {
                let read = crate::app::installed()
                    .and_then(|bundle| bundle.read(path))
                    .ok_or_else(|| format!("cannot find module '{}'", path))?;
                read.map(|bytes| (PathBuf::from(path), bytes))
                    .map_err(|e| format!("cannot read module '{}': {}", path, e))
            }
            Located::Disk(path) => {
                let bytes = std::fs::read(path)
                    .map_err(|e| format!("cannot read module '{}': {}", path.display(), e))?;
                Ok((path.clone(), bytes))
            }
        }
    }

    /// Resolves and reads module source or bytecode: from the running app's
    /// bundle when it has the module, otherwise from disk.
    pub fn read_module(&self, requested: &str) -> Result<(PathBuf, Vec<u8>), String> {
        self.read_located(&self.locate_module(requested)?)
    }

    /// Looks a module up in the app bundle (see `crate::app`) under the same
    /// names `resolve_module` tries on disk, in the same order: under the
    /// base directory first, then at the bundle root.
    fn find_bundled_module(&self, requested: &str) -> Option<String> {
        let bundle = crate::app::installed()?;
        let dotted = requested.replace('.', "/");
        let mut names: Vec<String> = self
            .searcher
            .patterns()
            .iter()
            .map(|pattern| pattern.replace('?', &dotted))
            .collect();
        if requested.ends_with(".nyk") || requested.ends_with(".nykb") {
            names.push(requested.to_string());
        } else {
            for suffix in [".nyk", ".nykb", "/init.nyk", "/mod.nyk", "/index.nyk"] {
                names.push(format!("{}{}", requested, suffix));
            }
        }
        let base = self
            .base_dir
            .as_deref()
            .map(|dir| dir.to_string_lossy().replace('\\', "/"))
            .filter(|dir| !dir.is_empty() && dir != ".");
        let mut candidates: Vec<String> = Vec::new();
        if let Some(base) = &base {
            candidates.extend(names.iter().map(|name| format!("{}/{}", base, name)));
        }
        candidates.extend(names);
        candidates
            .iter()
            .find_map(|candidate| match bundle.entry(candidate) {
                Some(crate::app::Entry::File { .. }) => crate::app::bundle::normalize(candidate),
                _ => None,
            })
    }

    /// Compiles module bytes into a verified `Proto`.
    pub fn compile_bytes(
        &mut self,
        path: &Path,
        bytes: &[u8],
        requested_name: &str,
    ) -> Result<Rc<Proto>, String> {
        let path_buf = path.to_path_buf();
        if let Some(cached) = self.cache.get_proto(&path_buf) {
            return Ok(cached);
        }

        let proto = if let Some(version) = crate::bytecode::deserialize::stale_version(bytes) {
            // Bytecode from another Neyuki: compile the source next to it
            // instead, so upgrading does not break modules shipped compiled.
            let source_path = path.with_extension("nyk");
            let Ok(source) = std::fs::read(&source_path) else {
                return Err(format!(
                    "cannot load module '{}' from {}: {}",
                    requested_name,
                    path.display(),
                    crate::bytecode::deserialize::version_error(version)
                ));
            };
            Self::compile_source(&source_path, &source, requested_name)?
        } else if bytes.starts_with(crate::bytecode::MAGIC)
            || path.extension().is_some_and(|ext| ext == "nykb")
        {
            let p = crate::bytecode::deserialize(bytes)
                .map_err(|e| format!("cannot load module '{}': {}", requested_name, e))?;
            crate::bytecode::verify_proto(&p)
                .map_err(|e| format!("bytecode verification failed: {}", e))?;
            p
        } else {
            Self::compile_source(path, bytes, requested_name)?
        };

        let rc = Rc::new(proto);
        self.cache.insert_proto(path_buf, rc.clone());
        Ok(rc)
    }

    fn compile_source(path: &Path, bytes: &[u8], requested_name: &str) -> Result<Proto, String> {
        let src = std::str::from_utf8(bytes)
            .map_err(|_| format!("cannot read module '{}': invalid UTF-8", requested_name))?;
        crate::compiler::compile_program(
            src,
            &path.to_string_lossy(),
            crate::compiler::Options::RUN,
        )
        .map_err(|err| format!("in module '{}': {}", requested_name, err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_searcher_dot_conversion() {
        let searcher = SearcherEngine::new();
        assert!(searcher.patterns().contains(&"./?.nyk".to_string()));
        assert!(searcher.patterns().contains(&"./?/init.nyk".to_string()));
    }

    #[test]
    fn test_module_cache_cycle_detection() {
        let mut cache = ModuleCache::new();
        assert!(cache.begin_loading("a").is_ok());
        assert!(cache.begin_loading("b").is_ok());
        let cycle_err = cache.begin_loading("a").unwrap_err();
        assert!(cycle_err.contains("circular dependency detected"));
        cache.finish_loading("b");
        cache.finish_loading("a");
        assert!(cache.begin_loading("a").is_ok());
    }

    /// A directory of its own under the system temp directory.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("neyuki_loader_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Compiled `source` as bytecode in format `version`.
    fn bytecode_with_version(source: &str, version: u8) -> Vec<u8> {
        let mut bytes = crate::compiler::compile_source_to_bytecode(source, "old.nykb").unwrap();
        bytes[crate::bytecode::MAGIC.len()] = version;
        bytes
    }

    #[test]
    fn test_stale_bytecode_recompiles_from_source() {
        let dir = scratch_dir("stale_with_source");
        let compiled = dir.join("mod.nykb");
        let old = crate::bytecode::MIN_SUPPORTED_BYTECODE_VERSION - 1;
        std::fs::write(&compiled, bytecode_with_version("return 1", old)).unwrap();
        std::fs::write(dir.join("mod.nyk"), "return 2").unwrap();

        let mut loader = ModuleLoader::new();
        let bytes = std::fs::read(&compiled).unwrap();
        let proto = loader.compile_bytes(&compiled, &bytes, "mod").unwrap();
        let source = proto.source.as_deref().unwrap_or_default();
        assert!(source.ends_with("mod.nyk"), "compiled from {}", source);

        // The recompiled module is cached under the bytecode's path.
        let again = loader.compile_bytes(&compiled, &bytes, "mod").unwrap();
        assert!(Rc::ptr_eq(&proto, &again));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stale_bytecode_without_source_names_the_file() {
        let dir = scratch_dir("stale_alone");
        let compiled = dir.join("mod.nykb");
        let old = crate::bytecode::MIN_SUPPORTED_BYTECODE_VERSION - 1;
        std::fs::write(&compiled, bytecode_with_version("return 1", old)).unwrap();

        let mut loader = ModuleLoader::new();
        let bytes = std::fs::read(&compiled).unwrap();
        let err = loader.compile_bytes(&compiled, &bytes, "mod").unwrap_err();
        assert!(err.contains("cannot load module 'mod'"), "{}", err);
        assert!(err.contains("mod.nykb"), "{}", err);
        assert!(err.contains("an older Neyuki"), "{}", err);
        assert!(loader.cache().get_proto(&compiled).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_current_bytecode_loads_without_source() {
        let dir = scratch_dir("current");
        let compiled = dir.join("mod.nykb");
        let bytes = crate::compiler::compile_source_to_bytecode("return 1", "mod.nyk").unwrap();
        std::fs::write(&compiled, &bytes).unwrap();

        let mut loader = ModuleLoader::new();
        assert!(loader.compile_bytes(&compiled, &bytes, "mod").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_path_resolution_candidate_extensions() {
        // Non-existent module returns clean error
        let err = PathEngine::resolve_candidate("non_existent_12345", None).unwrap_err();
        assert!(matches!(err, PathResolutionError::FileNotFound(_)));
    }
}
