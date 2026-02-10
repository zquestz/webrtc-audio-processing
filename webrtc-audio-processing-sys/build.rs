use anyhow::{Context, Result, bail};
use bindgen::callbacks::{AttributeInfo, DeriveInfo, ParseCallbacks};
use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    process::Command,
};

/// Name and minimum version of the library that we are binding to.
const LIB_NAME: &str = "webrtc-audio-processing-2";
#[cfg(not(feature = "bundled"))]
const LIB_MIN_VERSION: &str = "2.1";

const MACOSX_DEPLOYMENT_TARGET_VAR: &str = "MACOSX_DEPLOYMENT_TARGET";

/// Symbol prefix for the webrtc-audio-processing library to allow multiple versions to coexist.
const SYMBOL_PREFIX: &str = "v2_";

fn out_dir() -> PathBuf {
    std::env::var("OUT_DIR").expect("OUT_DIR environment var not set.").into()
}

/// Prefix specified symbols in a static library using objcopy --redefine-sym.
fn prefix_archive_symbols(
    archive_path: &std::path::Path,
    symbols: &[String],
    prefix: &str,
) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }

    eprintln!(
        "Prefixing {} symbols in {} with '{}'",
        symbols.len(),
        archive_path.display(),
        prefix
    );

    let temp_path = archive_path.with_extension("prefixed.a");

    let objcopy = determine_objcopy_path()?;

    // Write arguments to a temp file to avoid "Argument list too long" errors.
    let args_path = archive_path.with_extension("args");
    let mut writer = BufWriter::new(File::create(&args_path)?);
    for symbol in symbols {
        writeln!(writer, "--redefine-sym={}={}{}", symbol, prefix, symbol)?;
    }
    writer.flush()?;
    drop(writer);

    let mut cmd = Command::new(&objcopy);
    cmd.arg(format!("@{}", args_path.display()));
    cmd.arg(archive_path);
    cmd.arg(&temp_path);

    eprintln!("Running {cmd:?}");
    let status = cmd.status().context(format!("Failed to execute {:?}", objcopy))?;

    if !status.success() {
        anyhow::bail!("{:?} failed with status: {}", objcopy, status);
    }

    std::fs::rename(&temp_path, archive_path).with_context(|| {
        format!("Failed to rename {} to {}", temp_path.display(), archive_path.display())
    })?;

    Ok(())
}

#[cfg(not(feature = "bundled"))]
mod webrtc {
    use super::*;

    pub(super) fn get_build_paths() -> Result<(Vec<PathBuf>, Vec<PathBuf>, bool)> {
        let (pkgconfig_include_path, pkgconfig_lib_path) = find_pkgconfig_paths()?;

        let include_path = std::env::var("WEBRTC_AUDIO_PROCESSING_INCLUDE")
            .ok()
            .map(PathBuf::from)
            .or(pkgconfig_include_path);
        let lib_path = std::env::var("WEBRTC_AUDIO_PROCESSING_LIB")
            .ok()
            .map(PathBuf::from)
            .or(pkgconfig_lib_path);

        if include_path.is_none() || lib_path.is_none() {
            bail!(
                "Couldn't find {}. Please install it or set WEBRTC_AUDIO_PROCESSING_INCLUDE and WEBRTC_AUDIO_PROCESSING_LIB environment variables.",
                LIB_NAME
            );
        }

        Ok((vec![include_path.unwrap()], vec![lib_path.unwrap()], false))
    }

    pub(super) fn build_if_necessary() -> Result<()> {
        Ok(())
    }

    fn find_pkgconfig_paths() -> Result<(Option<PathBuf>, Option<PathBuf>)> {
        let lib = match pkg_config::Config::new()
            .atleast_version(LIB_MIN_VERSION)
            .statik(false)
            .probe(LIB_NAME)
        {
            Ok(lib) => lib,
            Err(e) => {
                eprintln!("Couldn't find {LIB_NAME} with pkg-config:");
                eprintln!("{e}");
                return Ok((None, None));
            },
        };

        Ok((lib.include_paths.first().cloned(), lib.link_paths.first().cloned()))
    }

    pub(super) fn prefix_library_symbols(
        _lib_dirs: &[PathBuf],
        _prefix: &str,
    ) -> Result<Vec<String>> {
        // For non-bundled builds, we can't prefix symbols in the system library.
        // Users would need to build with bundled feature for multi-version support.
        println!(
            "cargo:warning=Symbol prefixing is only supported with the 'bundled' feature. \
            Without it, linking multiple versions of this crate may cause symbol conflicts."
        );

        Ok(vec![])
    }
}

#[cfg(feature = "bundled")]
mod webrtc {
    use super::*;
    use std::{collections::HashSet, path::Path};

    const BUNDLED_SOURCE_PATH: &str = "./webrtc-audio-processing";

    /// Returns (include_paths, lib_paths, has_system_abseil).
    pub(super) fn get_build_paths() -> Result<(Vec<PathBuf>, Vec<PathBuf>, bool)> {
        let mut include_paths = vec![
            out_dir().join("include"),
            out_dir().join("include").join(LIB_NAME),
            webrtc_source_dir(),
            webrtc_source_dir().join("webrtc"),
        ];
        // TODO(strohel): instead of hardcoding the paths, we should consult the pkgconfig file that
        // the bundled webrtc-audio-processing build produces.
        let mut lib_paths = vec![
            // MacOS, Arch Linux, baseline default
            out_dir().join("lib"),
            // Gentoo Linux (x86_64 multilib)
            out_dir().join("lib64"),
        ];

        // Debian/Ubuntu multiarch path derived from target triple.
        // e.g., "x86_64-unknown-linux-gnu" → "lib/x86_64-linux-gnu"
        //        "aarch64-unknown-linux-gnu" → "lib/aarch64-linux-gnu"
        if let Ok(target) = std::env::var("TARGET") {
            let parts: Vec<&str> = target.split('-').collect();
            if parts.len() >= 3 {
                let arch = parts[0];
                let os_abi = parts[parts.len() - 2..].join("-");
                lib_paths.push(out_dir().join("lib").join(format!("{arch}-{os_abi}")));
            }
        }

        // Notes: c8896801 added support for 20250814, but the meson.build is still expecting
        // >=20240722 and the subproject will fetch 20240722. If the build environment has 20250814
        // installed, it should still pick it up and build successfully, though.
        let mut has_system_abseil =
            pkg_config::Config::new().atleast_version("20240722").probe("absl_base").ok();
        if let Some(ref mut lib) = has_system_abseil {
            // If abseil package is installed locally, meson would have linked it for
            // webrtc-audio-processing-2. Use the same library for our wrapper, too.
            include_paths.append(&mut lib.include_paths);
            lib_paths.append(&mut lib.link_paths);
        } else {
            // Otherwise use the local build fetched and built by meson.
            include_paths
                .push(webrtc_source_dir().join("subprojects").join("abseil-cpp-20240722.0"));
            lib_paths.push(webrtc_build_dir().join("subprojects").join("abseil-cpp-20240722.0"));
        }

        Ok((include_paths, lib_paths, has_system_abseil.is_some()))
    }

    pub(super) fn build_if_necessary() -> Result<()> {
        let bundled_source_path = Path::new(BUNDLED_SOURCE_PATH);
        if bundled_source_path.read_dir()?.next().is_none() {
            eprintln!("The webrtc-audio-processing source directory is empty.");
            eprintln!("See the crate README for installation instructions.");
            eprintln!("Remember to clone the repo recursively if building from source.");
            bail!("Aborting compilation because bundled source directory is empty.");
        }

        let webrtc_source_dir = webrtc_source_dir();
        let webrtc_build_dir = webrtc_build_dir();
        eprintln!(
            "Copying webrtc-audio-processing to {} and building it in {}",
            webrtc_source_dir.display(),
            webrtc_build_dir.display()
        );

        // Copy the sources to under out directory so that we can patch it without consequences.
        // A Rust copy rather than `cp -a`: Windows has no `cp`, and cargo's git checkout embeds
        // a real `.git` directory in the submodule whose read-only pack files make re-copies
        // into a cached OUT_DIR fail.
        copy_dir_recursive(bundled_source_path, &webrtc_source_dir)?;

        #[cfg(feature = "experimental-unlink-ns")]
        apply_patch("unlink-multichannel-noise-suppression-filters.patch")?;

        patch_denormal_disabler(&webrtc_source_dir)?;

        let mut meson = Command::new("meson");
        meson.arg("setup").arg("--prefix").arg(out_dir().as_os_str());

        // Only use --reconfigure if a prior build exists (has meson-private dir).
        // On fresh builds (e.g., CI without cache), --reconfigure fails because
        // there's no existing configuration to reconfigure.
        if webrtc_build_dir.join("meson-private").exists() {
            meson.arg("--reconfigure");
        }

        if cfg!(target_os = "macos") {
            let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
            let host_arch = std::env::consts::ARCH;
            let is_cross = target_arch != host_arch;

            let mut link_args = vec![
                "'-framework', 'CoreFoundation'".to_string(),
                "'-framework', 'Foundation'".to_string(),
            ];

            if is_cross {
                let arch_arg = format!("'-arch', '{target_arch}'");
                meson.arg(format!("-Dc_args=[{arch_arg}]"));
                meson.arg(format!("-Dcpp_args=[{arch_arg}]"));
                link_args.push(arch_arg);

                let cross_file = out_dir().join("meson-cross.ini");
                std::fs::write(
                    &cross_file,
                    format!(
                        "\
[binaries]
c = 'clang'
cpp = 'clang++'
ar = 'ar'
strip = 'strip'

[host_machine]
system = 'darwin'
cpu_family = '{target_arch}'
cpu = '{target_arch}'
endian = 'little'
"
                    ),
                )?;
                meson.arg("--cross-file");
                meson.arg(cross_file.to_str().unwrap());
            }

            let link_args_str = link_args.join(", ");
            meson.arg(format!("-Dc_link_args=[{link_args_str}]"));
            meson.arg(format!("-Dcpp_link_args=[{link_args_str}]"));
        }

        if cfg!(target_os = "windows") {
            // Activate Visual Studio environment so meson finds MSVC (cl.exe, link.exe)
            meson.arg("--vsenv");

            // Write MSVC native file with required defines for abseil/WebRTC headers
            let native_file = out_dir().join("msvc-native.ini");
            std::fs::write(
                &native_file,
                "\
[binaries]
c = 'cl'
cpp = 'cl'
ar = 'lib'

[built-in options]
cpp_std = 'c++20'
cpp_eh = 'sc'
c_args = ['-DWIN32_LEAN_AND_MEAN', '-DNOMINMAX']
cpp_args = ['-DWIN32_LEAN_AND_MEAN', '-DNOMINMAX']
",
            )?;
            meson.arg("--native-file");
            meson.arg(native_file.to_str().unwrap());
        }

        let status = meson
            .arg("-Ddefault_library=static")
            .arg(webrtc_build_dir.as_os_str())
            .arg(webrtc_source_dir.as_os_str())
            .status()
            .context("Failed to execute meson. Do you have it installed?")?;
        assert!(status.success(), "Command failed: {:?}", &meson);

        let mut compile = Command::new("meson");
        let status = compile
            .args(["compile", "-C"])
            .arg(webrtc_build_dir.to_str().unwrap())
            .status()
            .context("Failed to execute meson compile")?;
        assert!(status.success(), "Command failed: {:?}", &compile);

        let mut install = Command::new("meson");
        let status = install
            .args(["install", "-C"])
            .arg(webrtc_build_dir.to_str().unwrap())
            .status()
            .context("Failed to execute meson install")?;
        assert!(status.success(), "Command failed: {:?}", &install);

        Ok(())
    }

    // Patch with `patch`.
    #[cfg(feature = "experimental-unlink-ns")]
    fn apply_patch(patch_name: &str) -> Result<()> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let patch = manifest.join("patches").join(patch_name);

        let status = Command::new("patch")
            .args(["-p1", "--forward"])
            .arg("-i")
            .arg(&patch)
            .current_dir(webrtc_source_dir())
            .status()
            .context("Failed to execute patch")?;

        anyhow::ensure!(status.success(), "Patch '{}' failed with status: {}", patch_name, status);
        Ok(())
    }

    /// Prefix symbols in the built webrtc-audio-processing static library.
    /// Returns the list of symbols that were renamed.
    pub(super) fn prefix_library_symbols(
        lib_dirs: &[PathBuf],
        prefix: &str,
    ) -> Result<Vec<String>> {
        if cfg!(target_os = "windows") {
            // Symbol prefixing via nm/objcopy is not available on MSVC.
            // Not needed when only one version of the library is linked.
            return Ok(vec![]);
        }

        let static_lib_filename = format!("lib{LIB_NAME}.a");

        for lib_dir in lib_dirs {
            let lib_path = lib_dir.join(&static_lib_filename);
            if lib_path.exists() {
                let symbols = get_defined_symbols(&lib_path)?;
                prefix_archive_symbols(&lib_path, &symbols, prefix)?;
                return Ok(symbols);
            }
        }

        bail!("Cannot find {static_lib_filename} in {lib_dirs:?} to prefix its symbols.");
    }

    /// Recursive source-tree copy used on all platforms (Windows has no
    /// `cp`). Skips `.git` — the build doesn't need repo metadata, and its
    /// read-only pack files can't be overwritten when re-copying into a
    /// cached OUT_DIR — and files whose destination already exists with
    /// the same size and a newer-or-equal mtime, so unchanged sources keep
    /// their old timestamps and ninja can stay incremental across build
    /// script reruns.
    fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
        std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        for entry in from.read_dir().with_context(|| format!("reading {}", from.display()))? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            let src = entry.path();
            let dst = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_dir_recursive(&src, &dst)?;
            } else {
                if let (Ok(src_meta), Ok(dst_meta)) = (entry.metadata(), dst.metadata()) {
                    let dst_up_to_date = src_meta.len() == dst_meta.len()
                        && match (src_meta.modified(), dst_meta.modified()) {
                            (Ok(src_time), Ok(dst_time)) => dst_time >= src_time,
                            _ => false,
                        };
                    if dst_up_to_date {
                        continue;
                    }
                }
                std::fs::copy(&src, &dst)
                    .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
            }
        }
        Ok(())
    }

    // MSVC has no GCC-style inline asm on ARM64. Upstream's denormal
    // disabler gates its x86 paths on `__clang__` (so MSVC x64 already
    // takes the no-op fallback) but assumes every ARM compiler accepts
    // `asm volatile`. Mirror the x86 compiler gate on the ARM arm so
    // MSVC arm64 also falls back to the no-op DenormalDisabler. The
    // patched gate preprocesses identically under GCC/clang, so this is
    // applied on every platform.
    fn patch_denormal_disabler(webrtc_source_dir: &Path) -> Result<()> {
        const UNPATCHED: &str = "#if defined(WEBRTC_DENORMAL_DISABLER_X86_SUPPORTED) || \\\n    defined(WEBRTC_ARCH_ARM_FAMILY)\n";
        const PATCHED: &str = "#if defined(WEBRTC_DENORMAL_DISABLER_X86_SUPPORTED) || (defined(WEBRTC_ARCH_ARM_FAMILY) && (defined(__GNUC__) || defined(__clang__)))\n";

        let path = webrtc_source_dir
            .join("webrtc")
            .join("system_wrappers")
            .join("source")
            .join("denormal_disabler.cc");
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if source.contains(PATCHED) {
            return Ok(());
        }
        anyhow::ensure!(
            source.contains(UNPATCHED),
            "did not find the denormal disabler support gate to patch in {}",
            path.display()
        );
        std::fs::write(&path, source.replacen(UNPATCHED, PATCHED, 1))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn webrtc_source_dir() -> PathBuf {
        out_dir().join("webrtc-audio-processing")
    }

    fn webrtc_build_dir() -> PathBuf {
        // Windows MAX_PATH (260 chars) is easily exceeded because cl.exe
        // concatenates CWD + relative source path BEFORE normalizing ".."
        // components. Use the shortest possible build path (C:\w = 3 chars)
        // to maximize headroom. Abseil has filenames up to 43 chars
        // (hashtablez_sampler_force_weak_definition.cc).
        // Falls back to the out_dir-based path if the short path can't be created.
        if cfg!(target_os = "windows") {
            let short = PathBuf::from("C:\\w");
            match std::fs::create_dir_all(&short) {
                Ok(()) => return short,
                Err(e) => {
                    eprintln!(
                        "Warning: Could not create short build path {}: {e}",
                        short.display()
                    );
                    eprintln!("Falling back to out_dir (may hit MAX_PATH issues)");
                },
            }
        }
        out_dir().join("webrtc-audio-processing-build")
    }

    /// Extract defined (non-external) symbols from a static library using nm.
    fn get_defined_symbols(archive_path: &std::path::Path) -> Result<Vec<String>> {
        let output = Command::new("nm")
            .arg("--defined-only")
            .arg("--format=posix")
            .arg(archive_path)
            .output()
            .context("Failed to execute nm")?;

        if !output.status.success() {
            anyhow::bail!("nm failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut symbols = HashSet::new();

        for line in stdout.lines() {
            // POSIX format: "symbol_name type value size"
            // We just need the first field (symbol name)
            if let Some(symbol) = line.split_whitespace().next() {
                symbols.insert(symbol.to_string());
            }
        }

        Ok(symbols.into_iter().collect())
    }
}

#[derive(Debug)]
struct CustomDeriveCallbacks;

impl ParseCallbacks for CustomDeriveCallbacks {
    fn add_derives(&self, info: &DeriveInfo) -> Vec<String> {
        // Matches EchoCanceller3Config, EchoCanceller3Config_Suppressor etc
        if info.name.starts_with("EchoCanceller3Config") && cfg!(feature = "serde") {
            vec!["serde::Deserialize".into(), "serde::Serialize".into()]
        // Matches AudioProcessing_Config, AudioProcessing_Config_EchoCanceller etc
        } else if info.name.starts_with("AudioProcessing_Config") {
            // Only derive Default for AudioProcessing_Config and its inner structs. bindgen Default
            // implementation ignores C/C++ struct default values and thus misleading to enable
            // globally. Note that we don't expose these defaults on `webrtc-audio-processing`
            // level: they are needed only by the code that converts from prettified Rust config
            // structs into their FFI variants to construct disabled/dummy values.
            vec!["Default".into()]
        } else {
            vec![]
        }
    }

    fn add_attributes(&self, info: &AttributeInfo<'_>) -> Vec<String> {
        if info.name.starts_with("EchoCanceller3Config") {
            // Prohibit construction of ffi EchoCanceller3Config and its children structs.
            // The only allowed API is through the wrapper struct in the webrtc_audio_processing crate.
            vec!["#[non_exhaustive]".into()]
        } else {
            vec![]
        }
    }
}

fn main() -> Result<()> {
    webrtc::build_if_necessary()?;
    let (include_dirs, lib_dirs, has_system_abseil) = webrtc::get_build_paths()?;

    // Prefix defined symbols in the webrtc library (bundled builds only)
    // Returns the list of renamed symbols to update wrapper references later
    let renamed_symbols = webrtc::prefix_library_symbols(&lib_dirs, SYMBOL_PREFIX)?;

    for dir in &lib_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }

    let mut cc_build = cc::Build::new();

    if cfg!(feature = "experimental-aec3-config") {
        cc_build.define("WEBRTC_AEC3_CONFIG", None);
    }

    // Set macos minimum version
    if cfg!(target_os = "macos") {
        let min_version = match env::var(MACOSX_DEPLOYMENT_TARGET_VAR) {
            Ok(ver) => ver,
            Err(_) => {
                String::from(match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
                    "x86_64" => "10.10", // Using what I found here https://github.com/webrtc-uwp/chromium-build/blob/master/config/mac/mac_sdk.gni#L17
                    "aarch64" => "11.0", // Apple silicon started here.
                    arch => panic!("unknown arch: {}", arch),
                })
            },
        };

        // `cc` doesn't try to pick up on this automatically, but `clang` needs it to
        // generate a "correct" Objective-C symbol table which better matches XCode.
        // See https://github.com/h4llow3En/mac-notification-sys/issues/45.
        cc_build.flag(format!("-mmacos-version-min={}", min_version));
    }

    // This automatically emits "cargo:rustc-link-lib=static=webrtc_audio_processing_wrapper".
    // The wrapper library should be linked before webrtc-audio-processing-2, otherwise strict
    // linkers (like when passing -Wl,--as-needed) may discard the c++ library (automatically
    // added by cc) from the linking list, resulting in build failure.
    // The linking order should respect the dependency graph, i.e. wrapper -> webrtc-2.
    cc_build.cpp(true).file("src/wrapper.cpp").includes(&include_dirs);

    if cfg!(target_os = "windows") {
        cc_build
            .flag("/std:c++20")
            .flag("/EHsc")
            .flag("/W3")
            .define("WEBRTC_WIN", None)
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("NOMINMAX", None);
    } else {
        cc_build.flag("-std=c++17").flag("-Wno-unused-parameter");
    }

    // Inform wrapper code that headers for internal classes (ResidualEchoDetector) are available.
    #[cfg(feature = "bundled")]
    cc_build.define("WEBRTC_HAS_INTERNAL_HEADERS", None);

    cc_build.out_dir(out_dir()).compile("webrtc_audio_processing_wrapper");

    // The the cc and bindgen commands emit `cargo:rerun-if-env-changed=...`, and these deactivate
    // the default behavior to rerun if _any_ source file changes. So state these explicitly.
    // build.rs is always included and doesn't have to be specified.
    println!("cargo:rerun-if-changed=src/wrapper.hpp");
    println!("cargo:rerun-if-changed=src/wrapper.cpp");

    // Prefix the wrapper library's references to webrtc symbols to match the renamed webrtc library.
    let wrapper_lib = if cfg!(target_os = "windows") {
        out_dir().join("webrtc_audio_processing_wrapper.lib")
    } else {
        out_dir().join("libwebrtc_audio_processing_wrapper.a")
    };
    if wrapper_lib.exists() {
        prefix_archive_symbols(&wrapper_lib, &renamed_symbols, SYMBOL_PREFIX)?;
    }

    if cfg!(feature = "bundled") {
        println!("cargo:rustc-link-lib=static={LIB_NAME}");
        // Only link abseil separately when using system-installed abseil.
        // When abseil is built as a meson subproject, its objects are statically
        // linked into the webrtc-audio-processing library.
        if has_system_abseil {
            println!("cargo:rustc-link-lib=absl_strings");
        }
    } else {
        println!("cargo:rustc-link-lib=dylib={LIB_NAME}");
    }

    if cfg!(target_os = "windows") {
        println!("cargo:rustc-link-lib=winmm");
    }

    let binding_file = out_dir().join("bindings.rs");
    let mut builder = bindgen::Builder::default()
        .header("src/wrapper.hpp")
        .clang_args(&["-x", "c++", "-std=c++17", "-fparse-all-comments"])
        .generate_comments(true)
        .enable_cxx_namespaces()
        // Rust edition 2024 warns on usafe operations outside unsafe block, even in unsafe fns.
        .wrap_unsafe_ops(true);

    builder = builder
        // Transitive dependencies are automatically included.
        .allowlist_function("webrtc_audio_processing_wrapper::.*")
        .opaque_type("std::.*")
        .parse_callbacks(Box::new(CustomDeriveCallbacks))
        .derive_debug(true)
        // The default implementation ignores C++11's brace-or-equal-initializers,
        // and thus misleading to enable. See also CustomDeriveCallbacks.
        .derive_default(false)
        .derive_partialeq(true);
    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    builder
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(&binding_file)
        .expect("Couldn't write bindings!");

    Ok(())
}

/// Reliably determine a path to objcopy binary bundled with the active Rust toolchain (rust-objcopy)
fn determine_objcopy_path() -> Result<PathBuf> {
    // 1. Get the rustc command (this might be a path or just "rustc")
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());

    // 2. Ask rustc for the sysroot. This works even if RUSTC="rustc"
    let output = Command::new(&rustc)
        .arg("--print")
        .arg("sysroot")
        .output()
        .context("Failed to execute rustc to find sysroot")?;

    if !output.status.success() {
        bail!("Failed to get sysroot from rustc: {:?}", output);
    }

    let sysroot_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in sysroot")?;
    let sysroot = PathBuf::from(sysroot_str.trim());

    // 3. Construct the path: <sysroot>/lib/rustlib/<HOST_TRIPLE>/bin/rust-objcopy
    // We use HOST because that is where the compiler (and tools) are running.
    let host = env::var("HOST").context("HOST env var not found")?;

    let objcopy = sysroot.join("lib").join("rustlib").join(host).join("bin").join("rust-objcopy");

    // Optional: verification
    if !objcopy.exists() {
        println!("cargo:warning=rust-objcopy not found at {:?}", objcopy);
        println!(
            "cargo:warning=Ensure the 'llvm-tools' component is installed: 'rustup component add llvm-tools'"
        );
    }

    Ok(objcopy)
}
