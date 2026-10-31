use std::path::{Path, PathBuf};

fn c_files(dir: &Path, exclude: &[&str]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension().map(|x| x == "c").unwrap_or(false)
                && !exclude.contains(&p.file_name().unwrap().to_str().unwrap()))
            .then_some(p)
        })
        .collect();
    v.sort();
    v
}

fn main() {
    let speex = PathBuf::from("vendor/speexdsp-1.2.1");
    let opus = PathBuf::from("vendor/opus-1.5.2");

    // speexdsp: MDF echo canceller + preprocessor.
    // Kept in its own cc::Build: silk files `#include "arch.h"` too, and
    // speexdsp's arch.h must not shadow celt's. FFT backend is smallft for
    // the same reason (its kiss would clash with celt's kiss_fft symbols).
    let mut spx = cc::Build::new();
    spx.include(speex.join("include"))
        .include(speex.join("libspeexdsp"))
        .include("vendor/extra/speex/speex") // speexdsp_config_types.h
        .define("NDEBUG", None)
        .define("FLOATING_POINT", None)
        .define("EXPORT", "") // normally provided by speexdsp's configure
        .define("USE_SMALLFT", None)
        .flag_if_supported("-fno-strict-aliasing")
        .debug(true)
        .warnings(false);
    for f in ["mdf.c", "preprocess.c", "filterbank.c", "fftwrap.c", "smallft.c"] {
        spx.file(speex.join("libspeexdsp").join(f));
    }
    spx.compile("speexdsp_c");

    // libopus, float build (no configure: the standard embedded recipe)
    let mut op = cc::Build::new();
    op.include(opus.join("include"))
        .include(opus.join("celt"))
        .include(opus.join("silk"))
        .include(opus.join("silk/float"))
        .include(opus.join("src"))
        .define("NDEBUG", None)
        .define("OPUS_BUILD", None)
        .define("VAR_ARRAYS", None)
        .define("HAVE_LRINT", None)
        .define("HAVE_LRINTF", None)
        .define("PACKAGE_VERSION", "\"1.5.2\"")
        .flag_if_supported("-fno-strict-aliasing")
        .warnings(false);
    for f in c_files(&opus.join("celt"), &[]) {
        op.file(f);
    }
    for f in c_files(&opus.join("silk"), &[]) {
        op.file(f);
    }
    for f in c_files(&opus.join("silk/float"), &[]) {
        op.file(f);
    }
    for f in c_files(
        &opus.join("src"),
        &["opus_compare.c", "opus_demo.c", "repacketizer_demo.c"],
    ) {
        op.file(f);
    }
    op.compile("opus_c");

    // unix hosts need libm for the C code; mingw provides math in msvcrt
    let target = std::env::var("TARGET").unwrap();
    if target.contains("linux") || target.contains("darwin") {
        println!("cargo:rustc-link-lib=m");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
