//! 実行ファイルへアイコンを埋め込む (build script から `#[path]` で読み込む)。
//!
//! GPUI 版の実行ファイルは、そのままだと `.rsrc` を持たない。すると
//! エクスプローラー・タスクバー・Alt+Tab・スタートメニューのショートカットが
//! すべて Windows の既定アイコンになる。窓のアイコンも同じ理由で欠ける
//! (`gpui-pre-windows` の `load_icon` は自分自身のリソース ID 1 を `LoadImageW` で読むので、
//! リソースが無いと `HICON::default()` に落ちて、タイトルバーとタスクバーが空になる)。
//!
//! そこで `rc.exe` を呼んで `assets/icon.ico` を ID 1 のアイコンにした `.res` を作り、
//! リンカへ渡す。Tauri 版 (`desktop/src-tauri/`) が埋めているのと同じ絵を使う。
//!
//! `rc.exe` は Windows SDK にしか無い。`RC` 環境変数 → `PATH` → SDK の既定の場所、の順に探し、
//! 最後まで見つからなければ警告だけ出して続ける (アイコンは付かないがビルドは通る)。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `icon` はパッケージ直下からの相対パス (例: `assets/icon.ico`)。
pub fn embed(icon: &str) {
    println!("cargo:rerun-if-changed={icon}");
    println!("cargo:rerun-if-env-changed=RC");
    // Windows 以外に出すことは今のところ無いが、`rc.exe` も無いので素直に諦める。
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let source = manifest.join(icon);
    if !source.is_file() {
        println!(
            "cargo:warning=アイコンが見つかりません: {}",
            source.display()
        );
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe (Windows SDK) が見つかりません。実行ファイルにアイコンは付きません。");
        return;
    };
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    // `.rc` は ASCII で書く。アイコンの在り処は `/i` (コマンドライン経由) で渡すので、
    // 日本語を含むパス (この作業ディレクトリ) でも rc.exe の文字コードに左右されない。
    let script = out.join("app-icon.rc");
    let resource = out.join("app-icon.res");
    let name = source.file_name().expect("icon file name");
    // バージョン情報 (VERSIONINFO) も同じ .rc で埋め込む。
    // FileDescription がタスクマネージャー/ファイルプロパティの表示名になる。
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut vparts = version.split('.').filter_map(|p| p.parse::<u32>().ok());
    let fv = format!(
        "{},{},{},0",
        vparts.next().unwrap_or(0),
        vparts.next().unwrap_or(0),
        vparts.next().unwrap_or(0)
    );
    fs::write(
        &script,
        format!(
            concat!(
                "1 ICON \"{name}\"\r\n",
                "1 VERSIONINFO\r\n",
                "FILEVERSION {fv}\r\n",
                "PRODUCTVERSION {fv}\r\n",
                "FILEFLAGSMASK 0x3fL\r\n",
                "FILEFLAGS 0x0L\r\n",
                "FILEOS 0x40004L\r\n",
                "FILETYPE 0x1L\r\n",
                "FILESUBTYPE 0x0L\r\n",
                "BEGIN\r\n",
                "    BLOCK \"StringFileInfo\"\r\n",
                "    BEGIN\r\n",
                "        BLOCK \"040904b0\"\r\n",
                "        BEGIN\r\n",
                "            VALUE \"CompanyName\", \"Deka Hub\"\r\n",
                "            VALUE \"FileDescription\", \"Deka Hub\"\r\n",
                "            VALUE \"FileVersion\", \"{ver}\"\r\n",
                "            VALUE \"InternalName\", \"deka-hub\"\r\n",
                "            VALUE \"OriginalFilename\", \"deka-hub.exe\"\r\n",
                "            VALUE \"ProductName\", \"Deka Hub\"\r\n",
                "            VALUE \"ProductVersion\", \"{ver}\"\r\n",
                "        END\r\n",
                "    END\r\n",
                "    BLOCK \"VarFileInfo\"\r\n",
                "    BEGIN\r\n",
                "        VALUE \"Translation\", 0x409, 1200\r\n",
                "    END\r\n",
                "END\r\n"
            ),
            name = name.to_string_lossy(),
            fv = fv,
            ver = version,
        ),
    )
    .expect("write app-icon.rc");
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/i")
        .arg(source.parent().expect("icon directory"))
        .arg("/fo")
        .arg(&resource)
        .arg(&script)
        .current_dir(&manifest)
        .status();
    match status {
        Ok(status) if status.success() => {
            // リンカへの入力として渡す。`link.exe` は `.res` をそのまま飲み込む。
            println!("cargo:rustc-link-arg-bins={}", resource.display());
        }
        Ok(status) => panic!(
            "rc.exe が失敗しました (終了コード {:?}): {}",
            status.code(),
            rc.display()
        ),
        Err(error) => panic!("rc.exe を実行できません ({}): {error}", rc.display()),
    }
}

/// `RC` → `PATH` → Windows SDK の既定の場所 (`%ProgramFiles(x86)%\Windows Kits\10\bin\<版>\<arch>`)。
fn find_rc() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("RC").map(PathBuf::from) {
        if explicit.is_file() {
            return Some(explicit);
        }
    }
    if let Some(path) = env::var_os("PATH") {
        if let Some(found) = env::split_paths(&path)
            .map(|dir| dir.join("rc.exe"))
            .find(|c| c.is_file())
        {
            return Some(found);
        }
    }
    // ビルド環境によっては `WindowsSdkVerBinPath` が直接刺さっている (VS の開発者プロンプト)。
    if let Some(bin) = env::var_os("WindowsSdkVerBinPath").map(PathBuf::from) {
        for arch in arch_names() {
            let candidate = bin.join(arch).join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let roots = ["ProgramFiles(x86)", "ProgramFiles"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from);
    for root in roots {
        let bin = root.join("Windows Kits").join("10").join("bin");
        let Ok(entries) = fs::read_dir(&bin) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        // SDK は複数入っていることがある。版が新しいほうを採る (文字列比較だと 10.0.9 が 10.0.26 より後ろに来る)。
        versions.sort_by_key(|a| version_key(a));
        for version in versions.into_iter().rev() {
            for arch in arch_names() {
                let candidate = version.join(arch).join("rc.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// 目標の CPU に対応する SDK の bin の名前。x64 を先に見る (x64 の rc.exe はどの目標でも使える)。
fn arch_names() -> &'static [&'static str] {
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => &["arm64", "x64"],
        Ok("x86") => &["x86", "x64"],
        _ => &["x64"],
    }
}

fn version_key(path: &Path) -> Vec<u32> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.split('.')
                .map(|part| part.parse().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}
