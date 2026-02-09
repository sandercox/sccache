// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! clang-tidy support for sccache.
//!
//! clang-tidy is a static analysis tool, not a compiler. It produces
//! diagnostics output (stdout/stderr) rather than object files.
//!
//! ## Caching Strategy
//!
//! The cache key is computed from:
//! - clang-tidy binary digest
//! - `--dump-config` stdout
//! - Preprocessed source from the underlying compiler's `-E` invocation
//! - Contents of `--config-file=<path>` and `--load=<path>` arguments
//!
//! The cached output is:
//! - stdout (diagnostics)
//! - stderr (errors)
//! - exit code
//! - `--export-fixes` file (if specified)

use crate::compiler::args::*;
use crate::compiler::c::{ArtifactDescriptor, CCompilerImpl, CCompilerKind, ParsedArguments};
use crate::compiler::{
    CCompileCommand, Cacheable, CompileCommand, CompileCommandImpl, CompilerArguments, Language,
    SingleCompileCommand,
};
use crate::counted_array;
use crate::dist;
use crate::mock_command::{CommandCreatorSync, RunCommand};
use crate::server;
use crate::util::run_input_output;
use async_trait::async_trait;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};

use crate::errors::*;

/// A unit struct on which to implement `CCompilerImpl`.
#[derive(Clone, Debug)]
pub struct ClangTidy {
    pub version: Option<String>,
}

#[async_trait]
impl CCompilerImpl for ClangTidy {
    fn kind(&self) -> CCompilerKind {
        CCompilerKind::ClangTidy
    }

    fn plusplus(&self) -> bool {
        false
    }

    fn version(&self) -> Option<String> {
        self.version.clone()
    }

    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
        _env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<ParsedArguments> {
        parse_arguments(arguments, cwd)
    }

    #[allow(clippy::too_many_arguments)]
    async fn preprocess<T>(
        &self,
        creator: &T,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        _may_dist: bool,
        _rewrite_includes_only: bool,
        _preprocessor_cache_mode: bool,
    ) -> Result<process::Output>
    where
        T: CommandCreatorSync,
    {
        // For clang-tidy, "preprocessing" means running --dump-config to get
        // the effective configuration. This is what we hash to determine
        // cache hits.
        preprocess(creator, executable, parsed_args, cwd, env_vars).await
    }

    fn generate_compile_commands<T>(
        &self,
        path_transformer: &mut dist::PathTransformer,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        _rewrite_includes_only: bool,
    ) -> Result<(
        Box<dyn CompileCommand<T>>,
        Option<dist::CompileCommand>,
        Cacheable,
    )>
    where
        T: CommandCreatorSync,
    {
        generate_compile_commands(path_transformer, executable, parsed_args, cwd, env_vars)
    }
}

ArgData! {
    PassThrough(OsString),
    PassThroughFlag,
    Checks(OsString),
    Config(OsString),
    ConfigFile(PathBuf),
    LoadPlugin(PathBuf),
    ExportFixes(PathBuf),
    Fix,
    FixErrors,
    CompileCommandsDir(PathBuf),
}

use self::ArgData::*;

counted_array!(static ARGS: [ArgInfo<ArgData>; _] = [
    // All double-dash flags must come before single-dash flags
    take_arg!("--checks", OsString, Concatenated(b'='), Checks),
    take_arg!("--config", OsString, Concatenated(b'='), Config),
    take_arg!("--config-file", PathBuf, Concatenated(b'='), ConfigFile),
    take_arg!("--export-fixes", PathBuf, Concatenated(b'='), ExportFixes),
    take_arg!("--extra-arg", OsString, Concatenated(b'='), PassThrough),
    take_arg!("--extra-arg-before", OsString, Concatenated(b'='), PassThrough),
    flag!("--fix", Fix),
    flag!("--fix-errors", FixErrors),
    take_arg!("--header-filter", OsString, Concatenated(b'='), PassThrough),
    take_arg!("--line-filter", OsString, Concatenated(b'='), PassThrough),
    take_arg!("--load", PathBuf, Concatenated(b'='), LoadPlugin),
    flag!("--quiet", PassThroughFlag),
    take_arg!("--system-headers", OsString, Concatenated(b'='), PassThrough),
    flag!("--warnings-as-errors", PassThroughFlag),
    // Single-dash flags
    take_arg!("-checks", OsString, Concatenated(b'='), Checks),
    take_arg!("-config", OsString, Concatenated(b'='), Config),
    take_arg!("-config-file", PathBuf, Concatenated(b'='), ConfigFile),
    take_arg!("-export-fixes", PathBuf, Concatenated(b'='), ExportFixes),
    take_arg!("-extra-arg", OsString, Concatenated(b'='), PassThrough),
    take_arg!("-extra-arg-before", OsString, Concatenated(b'='), PassThrough),
    flag!("-fix", Fix),
    flag!("-fix-errors", FixErrors),
    take_arg!("-header-filter", OsString, Concatenated(b'='), PassThrough),
    take_arg!("-line-filter", OsString, Concatenated(b'='), PassThrough),
    take_arg!("-load", PathBuf, Concatenated(b'='), LoadPlugin),
    // Compile commands directory - uncacheable for POC
    take_arg!("-p", PathBuf, Concatenated(b'='), CompileCommandsDir),
    flag!("-quiet", PassThroughFlag),
    take_arg!("-system-headers", OsString, Concatenated(b'='), PassThrough),
    flag!("-warnings-as-errors", PassThroughFlag),
]);

/// Split `common_args` at the `--` separator.
///
/// Returns `(tidy_args, compiler_exe, compile_flags)` where:
/// - `tidy_args`: arguments before `--`
/// - `compiler_exe`: the first arg after `--` if it looks like a compiler path
/// - `compile_flags`: remaining arguments after `--` (excluding the compiler)
fn split_at_double_dash(
    common_args: &[OsString],
) -> (Vec<&OsString>, Option<&OsString>, Vec<&OsString>) {
    let mut tidy_args = vec![];
    let mut compiler_exe = None;
    let mut compile_flags = vec![];
    let mut seen_double_dash = false;
    let mut is_first_after_dd = true;

    for arg in common_args {
        if arg == "--" {
            seen_double_dash = true;
            is_first_after_dd = true;
            continue;
        }

        if !seen_double_dash {
            tidy_args.push(arg);
            continue;
        }

        if is_first_after_dd {
            is_first_after_dd = false;
            if is_likely_compiler_path(&arg.to_string_lossy()) {
                compiler_exe = Some(arg);
                continue;
            }
        }

        compile_flags.push(arg);
    }

    (tidy_args, compiler_exe, compile_flags)
}

/// Parse clang-tidy arguments.
pub fn parse_arguments(arguments: &[OsString], cwd: &Path) -> CompilerArguments<ParsedArguments> {
    let mut common_args = vec![];
    let mut extra_hash_files = vec![];
    let mut input_file: Option<PathBuf> = None;
    let mut export_fixes: Option<PathBuf> = None;
    let mut compile_flags: Vec<OsString> = vec![];
    let mut seen_double_dash = false;

    let mut it = ArgsIter::new(arguments.iter().cloned(), &ARGS[..]);

    loop {
        let arg = match it.next() {
            Some(Ok(arg)) => arg,
            Some(Err(ArgParseError::UnexpectedEndOfArgs)) => {
                return CompilerArguments::CannotCache(
                    "argument parse",
                    Some("Unexpected end of args".to_string()),
                );
            }
            Some(Err(ArgParseError::InvalidUnicode(arg))) => {
                return CompilerArguments::CannotCache(
                    "argument parse",
                    Some(format!("Couldn't parse argument {:?}", arg)),
                );
            }
            Some(Err(ArgParseError::Other(msg))) => {
                return CompilerArguments::CannotCache("argument parse", Some(msg.to_string()));
            }
            None => break,
        };

        match arg.get_data() {
            Some(Fix) | Some(FixErrors) => {
                // -fix and -fix-errors modify source files in place
                return CompilerArguments::CannotCache(
                    "-fix",
                    Some("clang-tidy -fix modifies source files".to_string()),
                );
            }
            Some(CompileCommandsDir(_)) => {
                // -p uses compile_commands.json, too complex for POC
                return CompilerArguments::CannotCache(
                    "-p",
                    Some("compile_commands.json not supported in POC".to_string()),
                );
            }
            Some(LoadPlugin(path)) => {
                // Hash the plugin .so file
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    cwd.join(path)
                };
                extra_hash_files.push(path);
                common_args.extend(arg.iter_os_strings());
            }
            Some(ConfigFile(path)) => {
                // Hash the config file
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    cwd.join(path)
                };
                extra_hash_files.push(path);
                common_args.extend(arg.iter_os_strings());
            }
            Some(ExportFixes(path)) => {
                // This is an output artifact
                export_fixes = Some(path.clone());
                common_args.extend(arg.iter_os_strings());
            }
            Some(Checks(_)) | Some(Config(_)) | Some(PassThrough(_)) | Some(PassThroughFlag) => {
                common_args.extend(arg.iter_os_strings());
            }
            None => {
                // The `--` separator arrives as `Argument::UnknownFlag("--")`;
                // `arg.flag_str()` returns `None` for `UnknownFlag`, so detect
                // it by comparing the OsString directly.
                if matches!(&arg, Argument::UnknownFlag(s) if s == "--") {
                    seen_double_dash = true;
                    continue;
                }

                if seen_double_dash {
                    // Everything after -- is compile flags (Raw or UnknownFlag)
                    compile_flags.extend(arg.iter_os_strings());
                } else {
                    // Before --, handle positional args and unknown flags
                    match &arg {
                        Argument::Raw(s) => {
                            if input_file.is_none() {
                                // Source file (first positional arg)
                                input_file = Some(PathBuf::from(s));
                            } else {
                                // Multiple source files - pass through
                                common_args.push(s.clone());
                            }
                        }
                        Argument::UnknownFlag(s) => {
                            // Unknown flag before --, pass through
                            common_args.push(s.clone());
                        }
                        _ => {
                            // Other argument types, pass through
                            common_args.extend(arg.iter_os_strings());
                        }
                    }
                }
            }
        }
    }

    let input = match input_file {
        Some(f) => f,
        None => {
            return CompilerArguments::CannotCache(
                "no source file",
                Some("No source file specified".to_string()),
            );
        }
    };

    // Determine language from file extension
    let language = Language::from_file_name(&input).unwrap_or(Language::Cxx);

    // Add compile flags to common_args (they affect the analysis)
    if !compile_flags.is_empty() {
        common_args.push(OsString::from("--"));
        common_args.extend(compile_flags);
    }

    // Build outputs map
    let mut outputs = HashMap::new();

    // If --export-fixes was specified, that's an output file
    if let Some(fixes_path) = export_fixes {
        outputs.insert(
            "fixes",
            ArtifactDescriptor {
                path: fixes_path,
                optional: true,
            },
        );
    }

    CompilerArguments::Ok(ParsedArguments {
        input,
        double_dash_input: false,
        language,
        compilation_flag: OsString::new(), // clang-tidy doesn't use -c
        depfile: None,
        outputs,
        dependency_args: vec![],
        preprocessor_args: vec![],
        common_args,
        arch_args: vec![],
        unhashed_args: vec![],
        extra_dist_files: vec![],
        extra_hash_files,
        msvc_show_includes: false,
        profile_generate: false,
        color_mode: crate::compiler::ColorMode::Auto,
        suppress_rewrite_includes_only: true,
        too_hard_for_preprocessor_cache_mode: Some(OsString::from("clang-tidy")),
    })
}

/// Run clang-tidy `--dump-config` and preprocess the source file. The combined
/// output is what we hash to determine cache hits. clang-tidy has no `-E` mode,
/// so `run_preprocessor` runs the underlying compiler from after `--`.
async fn preprocess<T>(
    creator: &T,
    executable: &Path,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
) -> Result<process::Output>
where
    T: CommandCreatorSync,
{
    // Step 1: Run clang-tidy --dump-config to get effective configuration
    let config_output = run_dump_config(creator, executable, parsed_args, cwd, env_vars).await?;

    // Step 2: Run the underlying compiler with -E/`/E` to get preprocessed
    // source. If the source has compilation errors the preprocessor exits
    // non-zero — keep going regardless, so the actual clang-tidy run can
    // surface the real diagnostics. The error output is folded into the hash
    // key, making the broken state cache-distinct.
    let preprocessed = match run_preprocessor(creator, executable, parsed_args, cwd, env_vars).await
    {
        Ok(output) => output,
        Err(err) => match err.downcast::<ProcessError>() {
            Ok(ProcessError(output)) => {
                debug!(
                    "clang-tidy preprocessor exited with status {:?}, continuing with available output",
                    output.status.code()
                );
                output
            }
            Err(err) => return Err(err),
        },
    };

    // Combine: config dump + preprocessed source
    let mut combined = config_output.stdout;
    combined.extend_from_slice(b"\n---PREPROCESSED---\n");
    combined.extend_from_slice(&preprocessed.stdout);

    // Include preprocessor stderr in the hash key — if the source has errors,
    // the error output makes the hash unique to this broken state.
    if !preprocessed.stderr.is_empty() {
        combined.extend_from_slice(b"\n---PREPROCESSOR-STDERR---\n");
        combined.extend_from_slice(&preprocessed.stderr);
    }

    // Combine stderr from both steps
    let mut stderr = config_output.stderr;
    if !preprocessed.stderr.is_empty() {
        if !stderr.is_empty() {
            stderr.extend_from_slice(b"\n");
        }
        stderr.extend_from_slice(&preprocessed.stderr);
    }

    Ok(process::Output {
        status: config_output.status,
        stdout: combined,
        stderr,
    })
}

/// Run clang-tidy --dump-config to get the effective configuration.
async fn run_dump_config<T>(
    creator: &T,
    executable: &Path,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
) -> Result<process::Output>
where
    T: CommandCreatorSync,
{
    let mut cmd = creator.clone().new_command_sync(executable);
    cmd.current_dir(cwd)
        .env_clear()
        .envs(env_vars.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Add dump-config flag
    cmd.arg("--dump-config");

    let (tidy_args, _, _) = split_at_double_dash(&parsed_args.common_args);

    // Add relevant arguments that affect configuration resolution.
    // --extra-arg / --extra-arg-before are critical: they carry flags like
    // --driver-mode=g++ that tell clang-tidy's internal clang how to
    // interpret the compile flags after `--`. Without them, clang-tidy
    // may fail to parse GCC-style flags on macOS (where the driver mode
    // cannot be auto-detected from the compiler path).
    for arg in &tidy_args {
        if affects_dump_config(&arg.to_string_lossy()) {
            cmd.arg(arg);
        }
    }

    // Add the source file - config resolution depends on the file path
    cmd.arg(&parsed_args.input);

    // Include compile flags after -- as they can affect checks
    let mut seen_double_dash = false;
    for arg in &parsed_args.common_args {
        if arg == "--" {
            seen_double_dash = true;
            cmd.arg(arg);
            continue;
        }
        if seen_double_dash {
            cmd.arg(arg);
        }
    }

    trace!("clang-tidy --dump-config: {:?}", cmd);
    run_input_output(cmd, None).await
}

/// Run the underlying compiler with `-E` (or `/E` in cl driver mode) to
/// preprocess the source file. clang-tidy itself is a static analyzer with
/// no `-E` mode, so we invoke the compiler named after `--` (e.g. cl.exe,
/// gcc, clang) with the same flags clang-tidy would receive, plus the
/// preprocess flag selected by the cl-driver-mode check below. The
/// preprocessed source bytes are folded into the cache key.
async fn run_preprocessor<T>(
    creator: &T,
    _executable: &Path,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
) -> Result<process::Output>
where
    T: CommandCreatorSync,
{
    let (tidy_args, compiler_exe, compile_flags) = split_at_double_dash(&parsed_args.common_args);

    // We need the underlying compiler to preprocess. If there's no compiler
    // after --, we can't preprocess.
    let compiler = match compiler_exe {
        Some(exe) => exe,
        None => bail!("clang-tidy preprocessing requires a compiler after --"),
    };

    let mut cmd = creator.clone().new_command_sync(compiler);
    cmd.current_dir(cwd)
        .env_clear()
        .envs(env_vars.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Check if MSVC driver mode is active (affects which preprocess flag to use)
    let is_driver_mode_cl = tidy_args.iter().any(|a| {
        let s = a.to_string_lossy();
        s.contains("--driver-mode=cl")
    }) || compiler
        .to_string_lossy()
        .to_lowercase()
        .ends_with("cl.exe");
    let preprocess_flag = if is_driver_mode_cl { "/E" } else { "-E" };

    // clang-tidy contract: --extra-arg-before precedes compile flags;
    // --extra-arg follows them. Misplacement breaks positional flags like -x.
    let mut extra_arg_before: Vec<String> = Vec::new();
    let mut extra_arg_after: Vec<String> = Vec::new();
    for arg in &tidy_args {
        let s = arg.to_string_lossy();
        if let Some(val) = s
            .strip_prefix("--extra-arg-before=")
            .or_else(|| s.strip_prefix("-extra-arg-before="))
        {
            extra_arg_before.push(val.to_owned());
        } else if let Some(val) = s
            .strip_prefix("--extra-arg=")
            .or_else(|| s.strip_prefix("-extra-arg="))
        {
            extra_arg_after.push(val.to_owned());
        }
    }

    for val in &extra_arg_before {
        cmd.arg(val);
    }

    let mut skip_next = false;
    for arg in &compile_flags {
        if skip_next {
            skip_next = false;
            continue;
        }

        let s = arg.to_string_lossy();

        if s == "-c" || s == "/c" {
            continue;
        }
        if s == "-o" {
            skip_next = true;
            continue;
        }
        if is_concatenated_output_flag(&s) {
            continue;
        }

        cmd.arg(*arg);
    }

    for val in &extra_arg_after {
        cmd.arg(val);
    }

    cmd.arg(preprocess_flag);
    cmd.arg(&parsed_args.input);

    trace!("preprocessing for clang-tidy: {:?}", cmd);
    run_input_output(cmd, None).await
}

/// Check if the first argument after `--` looks like a compiler path rather than a flag.
///
/// In `clang-tidy file.cpp -- [compiler] [flags...]`, the compiler is optional.
/// When present, it's a path like `C:\MSVC\cl.exe` or `/usr/bin/g++`.
/// When absent, the first thing after `--` is a flag like `-std=c++17` or `/W4`,
/// or a second source file like `b.cpp`.
fn is_likely_compiler_path(arg: &str) -> bool {
    // '-': flag. '@': response file.
    if arg.starts_with('-') || arg.starts_with('@') {
        return false;
    }
    // Guards against `clang-tidy a.cpp -- b.cpp` misclassifying b.cpp.
    if let Some(ext) = Path::new(arg).extension().and_then(|e| e.to_str()) {
        if matches!(
            ext.to_ascii_lowercase().as_str(),
            "c" | "cc" | "cpp" | "cxx" | "c++" | "h" | "hh" | "hpp" | "hxx" | "h++" | "m" | "mm"
        ) {
            return false;
        }
    }
    // Contains backslash → Windows path (e.g. C:\MSVC\cl.exe)
    if arg.contains('\\') {
        return true;
    }
    // Drive letter prefix → Windows path (e.g. C:/MSVC/cl.exe)
    if arg.len() >= 2 && arg.as_bytes()[0].is_ascii_alphabetic() && arg.as_bytes()[1] == b':' {
        return true;
    }
    // Starts with / → could be a Unix path or an MSVC-style flag
    if let Some(rest) = arg.strip_prefix('/') {
        // Unix paths have multiple / (like /usr/bin/gcc), MSVC flags don't
        return rest.contains('/');
    }
    // Bare name without flag prefix (e.g. "gcc", "cl.exe") → likely a compiler on PATH
    true
}

/// A compile command for clang-tidy that merges stderr into stdout.
///
/// cmake's `__run_co_compile --tidy=` always shows the tidy tool's stdout
/// but only shows stderr when the exit code is non-zero. Since clang-tidy
/// outputs its diagnostics to stderr, this means detailed error messages
/// can be hidden when running through cmake. By merging stderr into stdout,
/// we ensure cmake always displays the full diagnostic output.
#[derive(Debug)]
struct ClangTidyCompileCommand(SingleCompileCommand);

#[async_trait]
impl CompileCommandImpl for ClangTidyCompileCommand {
    fn get_executable(&self) -> PathBuf {
        self.0.get_executable()
    }
    fn get_arguments(&self) -> Vec<OsString> {
        self.0.get_arguments()
    }
    fn get_env_vars(&self) -> Vec<(OsString, OsString)> {
        self.0.get_env_vars()
    }
    fn get_cwd(&self) -> PathBuf {
        self.0.get_cwd()
    }

    async fn execute<T>(
        &self,
        service: &server::SccacheService<T>,
        creator: &T,
    ) -> Result<process::Output>
    where
        T: CommandCreatorSync,
    {
        // clang-tidy outputs diagnostics to stderr. Merge stderr into stdout
        // so that callers like cmake (which may hide stderr on exit code 0)
        // always see the full diagnostic output.
        match self.0.execute(service, creator).await {
            Ok(mut output) => {
                // clang-tidy prints noisy summary lines (warning counts,
                // header-filter suggestions) even on success. Suppress all
                // output on clean exit — only errors matter.
                output.stdout.clear();
                output.stderr.clear();
                Ok(output)
            }
            Err(err) => match err.downcast::<ProcessError>() {
                Ok(ProcessError(mut output)) => {
                    merge_stderr_into_stdout(&mut output);
                    Err(ProcessError(output).into())
                }
                Err(err) => Err(err),
            },
        }
    }
}

/// Merge stderr into stdout for clang-tidy output.
///
/// clang-tidy writes diagnostics to stderr. When invoked by cmake's
/// `__run_co_compile --tidy=`, cmake only shows stderr when the tidy
/// tool exits non-zero, but always shows stdout. By merging stderr
/// into stdout, we ensure diagnostic output is always visible.
fn merge_stderr_into_stdout(output: &mut process::Output) {
    if !output.stderr.is_empty() {
        if !output.stdout.is_empty() && !output.stdout.ends_with(b"\n") {
            output.stdout.push(b'\n');
        }
        output.stdout.extend_from_slice(&output.stderr);
        output.stderr.clear();
    }
}

/// Returns true for clang-tidy flags whose values change `--dump-config`
/// output. Short prefixes subsume long forms (`--config` covers `--config-file`,
/// `--extra-arg` covers `--extra-arg-before`).
fn affects_dump_config(arg: &str) -> bool {
    arg.starts_with("--checks")
        || arg.starts_with("-checks")
        || arg.starts_with("--config")
        || arg.starts_with("-config")
        || arg.starts_with("--extra-arg")
        || arg.starts_with("-extra-arg")
}

/// Concatenated output-file flags: `-o<file>`, `/Fo*`, `/Fe*`, `/Fi*`, `/Fd*`.
/// The bare `-o <file>` separated form is handled at each call site since it
/// requires consuming the following argument.
fn is_concatenated_output_flag(s: &str) -> bool {
    (s.starts_with("-o") && s.len() > 2)
        || s.starts_with("/Fo")
        || s.starts_with("/Fe")
        || s.starts_with("/Fi")
        || s.starts_with("/Fd")
}

/// Generate the actual clang-tidy command.
fn generate_compile_commands<T>(
    _path_transformer: &mut dist::PathTransformer,
    executable: &Path,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
) -> Result<(
    Box<dyn CompileCommand<T>>,
    Option<dist::CompileCommand>,
    Cacheable,
)>
where
    T: CommandCreatorSync,
{
    let mut args = vec![];

    // Add source file first
    args.push(parsed_args.input.clone().into_os_string());

    // Filter output flags (-o, /Fo, /Fd, /Fe, /Fi) from compile flags after --
    // — clang-tidy emits diagnostics to stdout, never objects/PDBs.
    let mut seen_double_dash = false;
    let mut skip_next = false;
    for arg in &parsed_args.common_args {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg_str = arg.to_string_lossy();

        if arg_str == "--" {
            seen_double_dash = true;
            args.push(arg.clone());
            continue;
        }

        if seen_double_dash {
            if arg_str == "-o" {
                skip_next = true;
                continue;
            }
            if is_concatenated_output_flag(&arg_str) {
                continue;
            }
        }

        args.push(arg.clone());
    }

    let command = ClangTidyCompileCommand(SingleCompileCommand {
        executable: executable.to_path_buf(),
        arguments: args,
        env_vars: env_vars.to_vec(),
        cwd: cwd.to_path_buf(),
    });

    // clang-tidy doesn't support distributed compilation
    Ok((CCompileCommand::new(command), None, Cacheable::Yes))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mock_command::*;
    use crate::test::utils::*;

    fn parse_args(args: &[&str]) -> CompilerArguments<ParsedArguments> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse_arguments(&args, &std::env::current_dir().unwrap())
    }

    #[test]
    fn test_parse_simple() {
        match parse_args(&["foo.cpp"]) {
            CompilerArguments::Ok(args) => {
                assert_eq!(args.input, PathBuf::from("foo.cpp"));
                assert_eq!(args.language, Language::Cxx);
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_with_checks() {
        match parse_args(&["-checks=-*,clang-analyzer-*", "foo.cpp"]) {
            CompilerArguments::Ok(args) => {
                assert_eq!(args.input, PathBuf::from("foo.cpp"));
                assert!(
                    args.common_args
                        .iter()
                        .any(|a| a.to_string_lossy().contains("checks"))
                );
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_with_compile_flags() {
        match parse_args(&["foo.cpp", "--", "-std=c++17", "-I/usr/include"]) {
            CompilerArguments::Ok(args) => {
                assert_eq!(args.input, PathBuf::from("foo.cpp"));
                // Compile flags should be in common_args after --
                let args_str: Vec<String> = args
                    .common_args
                    .iter()
                    .map(|s| s.to_string_lossy().to_string())
                    .collect();
                assert!(args_str.contains(&"--".to_string()));
                assert!(args_str.contains(&"-std=c++17".to_string()));
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_fix_uncacheable() {
        match parse_args(&["-fix", "foo.cpp"]) {
            CompilerArguments::CannotCache(reason, _) => {
                assert_eq!(reason, "-fix");
            }
            other => panic!("Expected CannotCache, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_fix_errors_uncacheable() {
        match parse_args(&["--fix-errors", "foo.cpp"]) {
            CompilerArguments::CannotCache(reason, _) => {
                assert_eq!(reason, "-fix");
            }
            other => panic!("Expected CannotCache, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_compile_commands_uncacheable() {
        match parse_args(&["-p=build", "foo.cpp"]) {
            CompilerArguments::CannotCache(reason, _) => {
                assert_eq!(reason, "-p");
            }
            other => panic!("Expected CannotCache, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_export_fixes() {
        match parse_args(&["--export-fixes=fixes.yaml", "foo.cpp"]) {
            CompilerArguments::Ok(args) => {
                assert_eq!(args.input, PathBuf::from("foo.cpp"));
                assert!(args.outputs.contains_key("fixes"));
                assert_eq!(
                    args.outputs.get("fixes").unwrap().path,
                    PathBuf::from("fixes.yaml")
                );
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_load_plugin() {
        match parse_args(&["--load=/path/to/plugin.so", "foo.cpp"]) {
            CompilerArguments::Ok(args) => {
                assert_eq!(args.input, PathBuf::from("foo.cpp"));
                assert!(
                    args.extra_hash_files
                        .iter()
                        .any(|p| p.to_string_lossy().contains("plugin.so"))
                );
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_no_source_file() {
        match parse_args(&["-checks=-*"]) {
            CompilerArguments::CannotCache(reason, _) => {
                assert_eq!(reason, "no source file");
            }
            other => panic!("Expected CannotCache, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_with_msvc_compile_flags() {
        // Simulate a typical Windows clang-tidy invocation with MSVC flags
        match parse_args(&[
            "--extra-arg-before=--driver-mode=cl",
            "foo.cpp",
            "--",
            "C:\\MSVC\\cl.exe",
            "/nologo",
            "/TP",
            "-DFOO",
            "-IC:\\include",
            "-external:IC:\\ext",
            "/W4",
            "/wd4100",
            "-std:c++20",
            "-MDd",
            "/Zi",
            "/showIncludes",
            "/Fofoo.obj",
            "/Fdfoo.pdb",
            "/FS",
            "-c",
            "foo.cpp",
        ]) {
            CompilerArguments::Ok(args) => {
                assert_eq!(args.input, PathBuf::from("foo.cpp"));
                // All compile flags should be in common_args after --
                let args_str: Vec<String> = args
                    .common_args
                    .iter()
                    .map(|s| s.to_string_lossy().to_string())
                    .collect();
                assert!(args_str.contains(&"--".to_string()));
                assert!(args_str.contains(&"--extra-arg-before=--driver-mode=cl".to_string()));
                assert!(args_str.contains(&"-DFOO".to_string()));
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_split_at_double_dash_with_compiler() {
        let args: Vec<OsString> = vec![
            "--checks=-*".into(),
            "--extra-arg=-Wno-error".into(),
            "--".into(),
            "C:\\MSVC\\cl.exe".into(),
            "/W4".into(),
            "-DFOO".into(),
        ];
        let (tidy_args, compiler, compile_flags) = split_at_double_dash(&args);
        assert_eq!(tidy_args.len(), 2);
        assert_eq!(compiler.unwrap().to_string_lossy(), "C:\\MSVC\\cl.exe");
        assert_eq!(compile_flags.len(), 2);
    }

    #[test]
    fn test_split_at_double_dash_without_compiler() {
        let args: Vec<OsString> = vec![
            "--checks=-*".into(),
            "--".into(),
            "-std=c++17".into(),
            "-DFOO".into(),
        ];
        let (tidy_args, compiler, compile_flags) = split_at_double_dash(&args);
        assert_eq!(tidy_args.len(), 1);
        assert!(compiler.is_none());
        assert_eq!(compile_flags.len(), 2);
    }

    #[test]
    fn test_split_at_double_dash_no_separator() {
        let args: Vec<OsString> = vec!["--checks=-*".into(), "--quiet".into()];
        let (tidy_args, compiler, compile_flags) = split_at_double_dash(&args);
        assert_eq!(tidy_args.len(), 2);
        assert!(compiler.is_none());
        assert!(compile_flags.is_empty());
    }

    #[test]
    fn test_affects_dump_config_forwards_extra_arg() {
        // Should affect dump_config
        assert!(affects_dump_config("--extra-arg-before=--driver-mode=g++"));
        assert!(affects_dump_config("-extra-arg-before=--driver-mode=g++"));
        assert!(affects_dump_config("--extra-arg=-Wno-error"));
        assert!(affects_dump_config("-extra-arg=-Wno-error"));
        assert!(affects_dump_config("--checks=-*"));
        assert!(affects_dump_config("-checks=-*"));
        assert!(affects_dump_config("--config-file=.clang-tidy"));
        assert!(affects_dump_config("--config=Checks: '*'"));

        // Should not affect dump_config
        assert!(!affects_dump_config("--quiet"));
        assert!(!affects_dump_config("--header-filter=.*"));
        assert!(!affects_dump_config("-system-headers"));
        assert!(!affects_dump_config("--warnings-as-errors=*"));
    }

    #[test]
    fn test_parse_preserves_extra_arg_before_for_dump_config() {
        match parse_args(&["--extra-arg-before=--driver-mode=g++", "foo.cpp"]) {
            CompilerArguments::Ok(args) => {
                let (tidy_args, _, _) = split_at_double_dash(&args.common_args);
                assert!(
                    tidy_args
                        .iter()
                        .any(|a| affects_dump_config(&a.to_string_lossy())
                            && a.to_string_lossy().contains("--driver-mode=g++"))
                );
            }
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_preprocess_swallows_preprocessor_process_error() {
        use std::sync::Arc;
        let creator = new_creator();
        // --dump-config succeeds, underlying -E fails.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), b"Checks: '*'", b"")),
        );
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(1), b"", b"fatal error: oops")),
        );

        let parsed = ParsedArguments {
            input: "foo.cpp".into(),
            double_dash_input: false,
            language: Language::Cxx,
            compilation_flag: OsString::new(),
            depfile: None,
            outputs: HashMap::new(),
            dependency_args: vec![],
            preprocessor_args: vec![],
            common_args: vec![
                OsString::from("--"),
                OsString::from("/usr/bin/g++"),
                OsString::from("-DFOO"),
            ],
            arch_args: vec![],
            unhashed_args: vec![],
            extra_dist_files: vec![],
            extra_hash_files: vec![],
            msvc_show_includes: false,
            profile_generate: false,
            color_mode: crate::compiler::ColorMode::Auto,
            suppress_rewrite_includes_only: true,
            too_hard_for_preprocessor_cache_mode: Some(OsString::from("clang-tidy")),
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(preprocess(
            &Arc::clone(&creator),
            Path::new("clang-tidy"),
            &parsed,
            Path::new("."),
            &[],
        ));

        let output = result.expect("preprocess must swallow preprocessor ProcessError");
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        assert!(stdout_str.contains("Checks: '*'"), "config dump in stdout");
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr_str.contains("fatal error: oops"),
            "preprocessor stderr surfaced to caller, got: {stderr_str}"
        );
    }

    #[test]
    fn test_post_dash_dash_source_file_not_treated_as_compiler() {
        let args: Vec<OsString> = vec!["a.cpp".into(), "--".into(), "b.cpp".into(), "-DFOO".into()];
        let common_args: Vec<OsString> = vec!["--".into(), "b.cpp".into(), "-DFOO".into()];
        let (_tidy_args, compiler, compile_flags) = split_at_double_dash(&common_args);
        assert!(compiler.is_none());
        let compile_flag_strs: Vec<String> = compile_flags
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(compile_flag_strs.contains(&"b.cpp".to_string()));
        assert!(compile_flag_strs.contains(&"-DFOO".to_string()));
        match parse_arguments(&args, Path::new(".")) {
            CompilerArguments::Ok(_) => {}
            other => panic!("Expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_is_likely_compiler_path_rejects_source_extensions() {
        // Arguments / sources
        assert!(!is_likely_compiler_path("b.cpp"));
        assert!(!is_likely_compiler_path("foo.cxx"));
        assert!(!is_likely_compiler_path("x.hpp"));
        assert!(!is_likely_compiler_path("X.HPP"));
        assert!(!is_likely_compiler_path("file.c"));
        assert!(!is_likely_compiler_path("Bar.mm"));
        assert!(!is_likely_compiler_path("@responses.rsp"));
        assert!(!is_likely_compiler_path("-std=c++17"));
        assert!(!is_likely_compiler_path("/W4"));

        // Compilers
        assert!(is_likely_compiler_path("gcc"));
        assert!(is_likely_compiler_path("cl"));
        assert!(is_likely_compiler_path("cl.exe"));
        assert!(is_likely_compiler_path("clang++"));
        assert!(is_likely_compiler_path("C:\\MSVC\\cl.exe"));
        assert!(is_likely_compiler_path("/usr/bin/g++"));
    }
}
