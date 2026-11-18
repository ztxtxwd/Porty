#!/usr/bin/env python3
"""Generate GPUI's ``shaders_bytes.rs`` when cross-compiling from Linux.

GPUI 0.2.2 only compiles its HLSL shaders when the *build host* is Windows
(its build script is gated on the host's `target_os`). When cross-compiling
`x86_64-pc-windows-gnu` from Linux, the first build fails with a missing
`OUT_DIR/shaders_bytes.rs`. Generate that file with a working Microsoft fxc
(fxc.exe under Wine works), then rebuild:

    cargo build -p pw-gui --release --target x86_64-pc-windows-gnu   # fails once
    GPUI_SRC=$(ls -d ~/.cargo/registry/src/*/gpui-0.2.2)
    OUT=$(ls -d target/x86_64-pc-windows-gnu/release/build/gpui-*/out)
    python3 scripts/gen_windows_shaders.py "$GPUI_SRC" "$OUT/shaders_bytes.rs" /path/to/fxc
    cargo build -p pw-gui --release --target x86_64-pc-windows-gnu

The generated file is build output; do not commit it.
"""

import os, re, subprocess, sys, tempfile

gpui_dir = sys.argv[1]
out_path = sys.argv[2]
fxc = sys.argv[3]

modules = [
    ("quad", "shaders.hlsl"),
    ("shadow", "shaders.hlsl"),
    ("path_rasterization", "shaders.hlsl"),
    ("path_sprite", "shaders.hlsl"),
    ("underline", "shaders.hlsl"),
    ("monochrome_sprite", "shaders.hlsl"),
    ("polychrome_sprite", "shaders.hlsl"),
    ("emoji_rasterization", "color_text_raster.hlsl"),
]

bindings = []
with tempfile.TemporaryDirectory() as tmp:
    for module, source_file in modules:
        source = os.path.join(gpui_dir, "src", "platform", "windows", source_file)
        for kind, target in (("vertex", "vs_4_1"), ("fragment", "ps_4_1")):
            entry = f"{module}_{kind}"
            var = f"{module.upper()}_{kind.upper()}_BYTES"
            header = os.path.join(tmp, f"{module}_{kind}.h")
            print(f"compiling {entry} ({target})", flush=True)
            result = subprocess.run(
                [fxc, "/T", target, "/E", entry, "/Fh", header, "/Vn", var, "/O3", source],
                capture_output=True,
                text=True,
            )
            if result.returncode != 0:
                sys.stderr.write(result.stdout + result.stderr)
                sys.exit(f"fxc failed for {entry}")
            content = open(header).read()
            start = content.find("const BYTE")
            if start < 0:
                sys.exit(f"no const BYTE in {header}")
            eq = content.find("=", start)
            definition = content[eq + 1 :].strip()
            definition = definition.replace("{", "[").replace("}", "]")
            bindings.append(f"const {var}: &[u8] = &{definition}\n")

with open(out_path, "w") as f:
    f.writelines(bindings)
print(f"wrote {out_path} ({len(bindings)} bindings)")
