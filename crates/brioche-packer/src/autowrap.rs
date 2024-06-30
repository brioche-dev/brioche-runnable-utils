use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{BufRead as _, Read as _, Write as _},
    path::{Path, PathBuf},
};

pub mod template;

use bstr::{ByteSlice as _, ByteVec as _};
use eyre::{Context as _, OptionExt as _};

#[derive(Debug, Clone)]
pub struct AutowrapConfig {
    pub recipe_path: PathBuf,
    pub paths: Vec<PathBuf>,
    pub globs: Vec<String>,
    pub quiet: bool,
    pub link_dependencies: Vec<PathBuf>,
    pub self_dependency: bool,
    pub dynamic_binary: Option<DynamicBinaryConfig>,
    pub shared_library: Option<SharedLibraryConfig>,
    pub script: Option<ScriptConfig>,
    pub rewrap: Option<RewrapConfig>,
}

#[derive(Debug, Clone)]
pub struct DynamicLinkingConfig {
    pub skip_libraries: HashSet<String>,
    pub extra_libraries: Vec<String>,
    pub skip_unknown_libraries: bool,
}

#[derive(Debug, Clone)]
pub struct DynamicBinaryConfig {
    pub packed_executable: PathBuf,
    pub dynamic_linking: DynamicLinkingConfig,
}

#[derive(Debug, Clone)]
pub struct SharedLibraryConfig {
    pub dynamic_linking: DynamicLinkingConfig,
}

#[derive(Debug, Clone)]
pub struct ScriptConfig {
    pub packed_executable: PathBuf,
    pub env: HashMap<String, runnable_core::EnvValue>,
    pub clear_env: bool,
}

#[derive(Debug, Clone)]
pub struct RewrapConfig {}

pub fn autowrap(config: &AutowrapConfig) -> eyre::Result<()> {
    let ctx = autowrap_context(config)?;

    for path in &config.paths {
        let path = config.recipe_path.join(path);
        let did_wrap = try_autowrap_path(&ctx, &path, &path)?;
        eyre::ensure!(did_wrap, "failed to wrap path: {path:?}");
        if !config.quiet {
            println!("wrapped {}", path.display());
        }
    }

    let mut globs = globset::GlobSetBuilder::new();
    for glob in &config.globs {
        globs.add(globset::Glob::new(glob)?);
    }

    let globs = globs.build()?;

    let walkdir = walkdir::WalkDir::new(&config.recipe_path);
    for entry in walkdir {
        let entry = entry?;
        if globs.is_match(entry.path()) {
            let did_wrap = try_autowrap_path(&ctx, entry.path(), entry.path())?;
            if !config.quiet {
                if did_wrap {
                    println!("wrapped {}", entry.path().display());
                } else {
                    println!("skipped {}", entry.path().display());
                }
            }
        }
    }

    Ok(())
}

struct AutowrapContext<'a> {
    config: &'a AutowrapConfig,
    resource_dir: PathBuf,
    all_resource_dirs: Vec<PathBuf>,
    link_dependencies: Vec<PathBuf>,
    link_dependency_library_paths: Vec<PathBuf>,
    link_dependency_paths: Vec<PathBuf>,
}

fn autowrap_context(config: &AutowrapConfig) -> eyre::Result<AutowrapContext> {
    // HACK: Workaround because finding a resource dir takes a program
    // path rather than a directory path, but then gets the parent path
    let program = config.recipe_path.join("program");

    let resource_dir = brioche_resources::find_output_resource_dir(&program)?;
    let all_resource_dirs = brioche_resources::find_resource_dirs(&program, true)?;

    let mut link_dependencies = vec![];
    if config.self_dependency {
        link_dependencies.push(config.recipe_path.to_owned());
    }
    link_dependencies.extend(config.link_dependencies.iter().cloned());

    let mut link_dependency_library_paths = vec![];
    let mut link_dependency_paths = vec![];
    for link_dep in &link_dependencies {
        // Add $LIBRARY_PATH directories from symlinks under
        // brioche-env.d/env/LIBRARY_PATH
        let library_path_env_dir = link_dep
            .join("brioche-env.d")
            .join("env")
            .join("LIBRARY_PATH");
        let library_path_env_dir_entries = match std::fs::read_dir(&library_path_env_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read directory {:?}", library_path_env_dir)
                });
            }
        };
        for entry in library_path_env_dir_entries {
            let entry = entry?;
            eyre::ensure!(
                entry.metadata()?.is_symlink(),
                "expected {:?} to be a symlink",
                entry.path()
            );

            let entry_path = entry
                .path()
                .canonicalize()
                .with_context(|| format!("failed to canonicalize path {:?}", entry.path()))?;
            link_dependency_library_paths.push(entry_path);
        }
    }

    for link_dep in &link_dependencies {
        // Add $PATH directories from symlinks under brioche-env.d/env/PATH
        let path_env_dir = link_dep.join("brioche-env.d").join("env").join("PATH");
        let path_env_dir_entries = match std::fs::read_dir(&path_env_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read directory {:?}", path_env_dir));
            }
        };
        for entry in path_env_dir_entries {
            let entry = entry?;
            eyre::ensure!(
                entry.metadata()?.is_symlink(),
                "expected {:?} to be a symlink",
                entry.path()
            );

            let entry_path = entry
                .path()
                .canonicalize()
                .with_context(|| format!("failed to canonicalize path {:?}", entry.path()))?;
            link_dependency_paths.push(entry_path);
        }
    }

    for link_dep in &link_dependencies {
        // Add bin/ to $PATH if it exists
        let link_dep_bin = link_dep.join("bin");
        if link_dep_bin.is_dir() {
            link_dependency_paths.push(link_dep_bin);
        }
    }

    Ok(AutowrapContext {
        config,
        resource_dir,
        all_resource_dirs,
        link_dependencies,
        link_dependency_library_paths,
        link_dependency_paths,
    })
}

fn try_autowrap_path(
    ctx: &AutowrapContext,
    source_path: &Path,
    output_path: &Path,
) -> eyre::Result<bool> {
    let Some(kind) = autowrap_kind(source_path)? else {
        return Ok(false);
    };

    match kind {
        AutowrapKind::DynamicBinary => autowrap_dynamic_binary(ctx, source_path, output_path),
        AutowrapKind::SharedLibrary => autowrap_shared_library(ctx, source_path, output_path),
        AutowrapKind::Script => autowrap_script(ctx, source_path, output_path),
        AutowrapKind::Rewrap => autowrap_rewrap(ctx, source_path, output_path),
    }
}

fn autowrap_kind(path: &Path) -> eyre::Result<Option<AutowrapKind>> {
    let contents = std::fs::read(path)?;

    let pack = brioche_pack::extract_pack(&contents[..]);

    if pack.is_ok() {
        Ok(Some(AutowrapKind::Rewrap))
    } else if contents.starts_with(b"#!") {
        Ok(Some(AutowrapKind::Script))
    } else {
        let program_object = goblin::Object::parse(&contents);

        let Ok(goblin::Object::Elf(program_object)) = program_object else {
            return Ok(None);
        };

        if program_object.interpreter.is_some() {
            Ok(Some(AutowrapKind::DynamicBinary))
        } else if program_object.is_lib {
            Ok(Some(AutowrapKind::SharedLibrary))
        } else {
            Ok(None)
        }
    }
}

enum AutowrapKind {
    DynamicBinary,
    SharedLibrary,
    Script,
    Rewrap,
}

fn autowrap_dynamic_binary(
    ctx: &AutowrapContext,
    source_path: &Path,
    output_path: &Path,
) -> eyre::Result<bool> {
    let Some(dynamic_binary_config) = &ctx.config.dynamic_binary else {
        return Ok(false);
    };

    let contents = std::fs::read(source_path)?;
    let program_object = goblin::Object::parse(&contents)?;

    let goblin::Object::Elf(program_object) = program_object else {
        eyre::bail!(
            "tried to wrap non-ELF dynamic binary: {}",
            source_path.display()
        );
    };

    let Some(interpreter) = program_object.interpreter else {
        eyre::bail!(
            "tried to wrap dynamic binary without an interpreter: {}",
            source_path.display()
        );
    };
    let relative_interpreter = interpreter.strip_prefix('/').ok_or_else(|| {
        eyre::eyre!("expected program interpreter to start with '/': {interpreter:?}")
    })?;

    let mut interpreter_path = None;
    for dependency in &ctx.link_dependencies {
        let dependency_path = dependency.join(relative_interpreter);
        if dependency_path.exists() {
            interpreter_path = Some(dependency_path);
            break;
        }
    }

    let interpreter_path = interpreter_path.ok_or_else(|| {
        eyre::eyre!("could not find interpreter for dynamic binary: {source_path:?}")
    })?;
    let interpreter_resource_path = add_named_blob_from(ctx, &interpreter_path)
        .with_context(|| format!("failed to add resource for interpreter {interpreter_path:?}"))?;
    let program_resource_path = add_named_blob_from(ctx, source_path)
        .with_context(|| format!("failed to add resource for program {source_path:?}"))?;

    let needed_libraries: VecDeque<_> = program_object
        .libraries
        .iter()
        .copied()
        .filter(|library| {
            !dynamic_binary_config
                .dynamic_linking
                .skip_libraries
                .contains(*library)
        })
        .chain(
            dynamic_binary_config
                .dynamic_linking
                .extra_libraries
                .iter()
                .map(|lib| &**lib),
        )
        .map(|lib| lib.to_string())
        .collect();

    let library_dir_resource_paths = collect_all_library_dirs(
        ctx,
        &dynamic_binary_config.dynamic_linking,
        needed_libraries,
    )?;

    let program = <Vec<u8>>::from_path_buf(program_resource_path)
        .map_err(|_| eyre::eyre!("invalid UTF-8 in path"))?;
    let interpreter = <Vec<u8>>::from_path_buf(interpreter_resource_path)
        .map_err(|_| eyre::eyre!("invalid UTF-8 in path"))?;
    let library_dirs = library_dir_resource_paths
        .into_iter()
        .map(|resource_path| {
            <Vec<u8>>::from_path_buf(resource_path)
                .map_err(|_| eyre::eyre!("invalid UTF-8 in path"))
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let pack = brioche_pack::Pack::LdLinux {
        program,
        interpreter,
        library_dirs,
        runtime_library_dirs: vec![],
    };

    let packed_exec_path = &dynamic_binary_config.packed_executable;
    let mut packed_exec = std::fs::File::open(packed_exec_path)
        .with_context(|| format!("failed to open packed executable {packed_exec_path:?}"))?;
    let mut output = std::fs::File::create(output_path)
        .with_context(|| format!("failed to create file {output_path:?}"))?;
    std::io::copy(&mut packed_exec, &mut output)
        .with_context(|| format!("failed to copy packed executable to {output_path:?}"))?;
    brioche_pack::inject_pack(output, &pack)
        .with_context(|| format!("failed to inject pack into {output_path:?}"))?;

    Ok(true)
}

fn autowrap_shared_library(
    ctx: &AutowrapContext,
    source_path: &Path,
    output_path: &Path,
) -> eyre::Result<bool> {
    let Some(shared_library_config) = &ctx.config.shared_library else {
        return Ok(false);
    };

    let contents = std::fs::read(source_path)?;
    let program_object = goblin::Object::parse(&contents)?;

    let goblin::Object::Elf(program_object) = program_object else {
        eyre::bail!(
            "tried to wrap non-ELF dynamic binary: {}",
            source_path.display()
        );
    };

    let needed_libraries: VecDeque<_> = program_object
        .libraries
        .iter()
        .copied()
        .filter(|library| {
            !shared_library_config
                .dynamic_linking
                .skip_libraries
                .contains(*library)
        })
        .chain(
            shared_library_config
                .dynamic_linking
                .extra_libraries
                .iter()
                .map(|lib| &**lib),
        )
        .map(|lib| lib.to_string())
        .collect();

    let library_dir_resource_paths = collect_all_library_dirs(
        ctx,
        &shared_library_config.dynamic_linking,
        needed_libraries,
    )?;

    let library_dirs = library_dir_resource_paths
        .into_iter()
        .map(|resource_path| {
            <Vec<u8>>::from_path_buf(resource_path)
                .map_err(|_| eyre::eyre!("invalid UTF-8 in path"))
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let pack = brioche_pack::Pack::Static { library_dirs };

    let file = if source_path == output_path {
        std::fs::OpenOptions::new().append(true).open(output_path)?
    } else {
        let mut new_file = std::fs::File::create(output_path)?;
        new_file.write_all(&contents)?;
        new_file
    };
    brioche_pack::inject_pack(file, &pack)?;

    Ok(true)
}

fn autowrap_script(
    ctx: &AutowrapContext,
    source_path: &Path,
    output_path: &Path,
) -> eyre::Result<bool> {
    let Some(script_config) = &ctx.config.script else {
        return Ok(false);
    };

    let script_file = std::fs::File::open(source_path)?;
    let mut script_file = std::io::BufReader::new(script_file);
    let mut shebang = [0; 2];
    let Ok(()) = script_file.read_exact(&mut shebang) else {
        return Ok(false);
    };
    if shebang != *b"#!" {
        return Ok(false);
    }

    let mut shebang_line = String::new();
    script_file.read_line(&mut shebang_line)?;

    let shebang_line = shebang_line.trim();
    let shebang_parts = shebang_line.split_once(|c: char| c.is_ascii_whitespace());
    let (command_path, arg) = match shebang_parts {
        Some((command_path, arg)) => (command_path.trim(), arg.trim()),
        None => (shebang_line, ""),
    };

    let mut arg = Some(arg).filter(|arg| !arg.is_empty());
    let mut command_name = command_path
        .split(|c: char| matches!(c, '/' | '\\'))
        .last()
        .unwrap_or(command_path);

    if command_name == "env" {
        command_name = arg.ok_or_eyre("expected argument for env script")?;
        arg = None;
    }
    let mut command = None;
    for link_dependency_path in &ctx.link_dependency_paths {
        if link_dependency_path.join(command_name).is_file() {
            command = Some(link_dependency_path.join(command_name));
            break;
        }
    }

    let command = command.ok_or_else(|| eyre::eyre!("could not find command {command_name:?}"))?;
    let command_resource = add_named_blob_from(ctx, &command)?;
    let script_resource = add_named_blob_from(ctx, source_path)?;

    let env_resource_paths = script_config
        .env
        .values()
        .filter_map(|value| match value {
            runnable_core::EnvValue::Clear => None,
            runnable_core::EnvValue::Inherit => None,
            runnable_core::EnvValue::Set { value } => Some(value),
            runnable_core::EnvValue::Fallback { value } => Some(value),
            runnable_core::EnvValue::Prepend {
                value,
                separator: _,
            } => Some(value),
            runnable_core::EnvValue::Append {
                value,
                separator: _,
            } => Some(value),
        })
        .flat_map(|template| &template.components)
        .filter_map(|component| match component {
            runnable_core::TemplateComponent::Literal { .. }
            | runnable_core::TemplateComponent::RelativePath { .. } => None,
            runnable_core::TemplateComponent::Resource { resource } => Some(
                resource
                    .to_path()
                    .map_err(|_| eyre::eyre!("invalid resource path")),
            ),
        })
        .collect::<eyre::Result<Vec<_>>>()?;

    let resource_paths = [command_resource.clone(), script_resource.clone()]
        .into_iter()
        .chain(env_resource_paths.into_iter().map(|path| path.to_owned()))
        .map(|path| {
            Vec::<u8>::from_path_buf(path).map_err(|_| eyre::eyre!("invalid resource path"))
        })
        .collect::<eyre::Result<Vec<_>>>()?;

    let command = runnable_core::Template::from_resource_path(command_resource)?;

    let mut args = vec![];
    if let Some(arg) = arg {
        args.push(runnable_core::ArgValue::Arg {
            value: runnable_core::Template::from_literal(arg.into()),
        });
    }
    args.push(runnable_core::ArgValue::Arg {
        value: runnable_core::Template::from_resource_path(script_resource.clone())?,
    });
    args.push(runnable_core::ArgValue::Rest);

    let env = script_config
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    let runnable_pack = runnable_core::Runnable {
        command,
        args,
        env,
        clear_env: script_config.clear_env,
        source: Some(runnable_core::RunnableSource {
            path: runnable_core::RunnablePath::from_resource_path(script_resource)?,
        }),
    };
    let pack = brioche_pack::Pack::Metadata {
        resource_paths,
        format: runnable_core::FORMAT.to_string(),
        metadata: serde_json::to_vec(&runnable_pack)?,
    };

    let packed_exec_path = &script_config.packed_executable;
    let mut packed_exec = std::fs::File::open(packed_exec_path)
        .with_context(|| format!("failed to open packed executable {packed_exec_path:?}"))?;

    let mut output = std::fs::File::create(output_path)
        .with_context(|| format!("failed to create file {output_path:?}"))?;
    std::io::copy(&mut packed_exec, &mut output)
        .with_context(|| format!("failed to copy packed executable to {output_path:?}"))?;
    brioche_pack::inject_pack(output, &pack)
        .with_context(|| format!("failed to inject pack into {output_path:?}"))?;

    Ok(true)
}

fn autowrap_rewrap(
    ctx: &AutowrapContext,
    source_path: &Path,
    _output_path: &Path,
) -> eyre::Result<bool> {
    let Some(_) = &ctx.config.rewrap else {
        return Ok(false);
    };

    eyre::bail!("tried to rewrap {source_path:?}, but rewrapping is not yet implemented");
}

fn collect_all_library_dirs(
    ctx: &AutowrapContext,
    dynamic_linking_config: &DynamicLinkingConfig,
    mut needed_libraries: VecDeque<String>,
) -> eyre::Result<Vec<PathBuf>> {
    let mut library_search_paths = ctx.link_dependency_library_paths.clone();
    let mut resource_library_dirs = vec![];
    let mut found_libraries = HashSet::new();
    let mut found_library_dirs = HashSet::new();

    while let Some(library_name) = needed_libraries.pop_front() {
        // If we've already found this library, then skip it
        if found_libraries.contains(&library_name) {
            continue;
        }

        // Find the path to the library
        let library_path = find_library(&library_search_paths, &library_name)?;
        let Some(library_path) = library_path else {
            if dynamic_linking_config.skip_unknown_libraries {
                continue;
            } else {
                eyre::bail!("library not found: {library_name:?}");
            }
        };

        found_libraries.insert(library_name.clone());

        // Don't add the library if it's been skipped. We still do everything
        // else so we can add transitive dependencies even if a library has
        // been skipped
        if !dynamic_linking_config
            .skip_libraries
            .contains(&*library_name)
        {
            // Add the library to the resource directory
            let library_resource_path = add_named_blob_from(ctx, &library_path)
                .with_context(|| format!("failed to add resource for library {library_path:?}"))?;

            // Add the parent dir to the list of library directories. Note
            // that this directory is guaranteed to only contain just this
            // library
            let library_resource_dir = library_resource_path
                .parent()
                .ok_or_eyre("failed to get resource parent dir")?
                .to_owned();

            let is_new_library_path = found_library_dirs.insert(library_resource_dir.clone());
            if is_new_library_path {
                resource_library_dirs.push(library_resource_dir.clone());
            }
        }

        // Try to get the dynamic dependencies from the library itself
        let Ok(library_file) = std::fs::read(&library_path) else {
            continue;
        };
        let Ok(library_object) = goblin::Object::parse(&library_file) else {
            continue;
        };

        // TODO: Support other object files
        let library_elf = match library_object {
            goblin::Object::Elf(elf) => elf,
            _ => {
                continue;
            }
        };
        needed_libraries.extend(library_elf.libraries.iter().map(|lib| lib.to_string()));

        // If the library has a Brioche pack, then use the included resources
        // for additional search directories
        if let Ok(library_pack) = brioche_pack::extract_pack(&library_file[..]) {
            let library_dirs = match &library_pack {
                brioche_pack::Pack::LdLinux { library_dirs, .. } => &library_dirs[..],
                brioche_pack::Pack::Static { library_dirs } => &library_dirs[..],
                brioche_pack::Pack::Metadata { .. } => &[],
            };

            for library_dir in library_dirs {
                let Ok(library_dir) = library_dir.to_path() else {
                    continue;
                };
                let Some(library_dir_path) =
                    brioche_resources::find_in_resource_dirs(&ctx.all_resource_dirs, library_dir)
                else {
                    continue;
                };

                library_search_paths.push(library_dir_path);
            }
        }
    }

    Ok(resource_library_dirs)
}

fn find_library(
    library_search_paths: &[PathBuf],
    library_name: &str,
) -> eyre::Result<Option<PathBuf>> {
    for path in library_search_paths {
        let lib_path = path.join(library_name);
        if lib_path.is_file() {
            return Ok(Some(lib_path));
        }
    }

    Ok(None)
}

fn add_named_blob_from(ctx: &AutowrapContext, path: &Path) -> eyre::Result<PathBuf> {
    use std::os::unix::prelude::PermissionsExt as _;

    let filename = path
        .file_name()
        .ok_or_eyre("failed to get filename from path")?;

    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;

    let permissions = metadata.permissions();
    let mode = permissions.mode();
    let is_executable = mode & 0o111 != 0;

    let mut contents = vec![];
    file.read_to_end(&mut contents)?;

    let resource_path = brioche_resources::add_named_blob(
        &ctx.resource_dir,
        std::io::Cursor::new(contents),
        is_executable,
        Path::new(filename),
    )?;
    Ok(resource_path)
}
