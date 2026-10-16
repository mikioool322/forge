# Dot-source this file before running cargo on Windows without Visual Studio:
#
#     . .\scripts\dev-env.ps1
#     cargo build --workspace
#
# It puts rustup's bin directory and the portable LLVM-MinGW toolchain on
# PATH. LLVM-MinGW provides:
#   * the C compiler for bundled SQLite (`cc` crate),
#   * `dlltool` used by rustc to generate raw-dylib import libraries.
# The linker itself is pinned to the toolchain's own `rust-lld` in
# `.cargo/config.toml`, so LLVM-MinGW's gcc driver is never used for linking.

$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH

$mingw = Get-ChildItem "$env:USERPROFILE\tools\llvm-mingw" -Directory -ErrorAction SilentlyContinue |
    ForEach-Object { Join-Path $_.FullName "bin" } |
    Where-Object { Test-Path $_ } |
    Select-Object -First 1

if ($mingw) {
    $env:PATH = "$mingw;" + $env:PATH
    $env:CC_x86_64_pc_windows_gnu = Join-Path $mingw "x86_64-w64-mingw32-gcc.exe"
    $env:AR_x86_64_pc_windows_gnu = Join-Path $mingw "llvm-ar.exe"
    $env:RANLIB_x86_64_pc_windows_gnu = Join-Path $mingw "llvm-ranlib.exe"
    Write-Output "forge dev-env: cc=$mingw"
} else {
    Write-Output "forge dev-env: WARNING - LLVM-MinGW not found; bundled SQLite will not build"
}
Write-Output "forge dev-env: rustc=$((Get-Command rustc -ErrorAction SilentlyContinue).Source)"
