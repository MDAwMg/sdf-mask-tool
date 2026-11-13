// 把 assets/icon/sdf-mask-tool.ico 作为 Windows 资源嵌入可执行文件，
// 使资源管理器、任务栏固定项和快捷方式都显示该图标。
//
// 做法是生成一个只含图标行的 .rc，再调用 Windows SDK 的 rc.exe 编译成 .res，
// 交给链接器作为输入对象。不引入 winres / embed-resource 之类的构建依赖。
//
// rc.exe 的查找顺序：环境变量 RC → PATH → Windows Kits 各版本目录（取最高版本）。
// 找不到或编译失败时只输出 cargo:warning，不阻断构建，可执行文件仅缺少资源图标。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// 在 Windows Kits 的安装目录里找 rc.exe，取最高版本的 x64 工具链。
fn find_rc_in_kits() -> Option<PathBuf> {
    let roots = [
        PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin"),
        PathBuf::from(r"C:\Program Files\Windows Kits\10\bin"),
    ];
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();

    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // 版本目录形如 10.0.22621.0，按数值比较取最高
            let key: u64 = name
                .split('.')
                .filter_map(|part| part.parse::<u64>().ok())
                .fold(0, |acc, n| acc.wrapping_mul(1000).wrapping_add(n));
            let rc = entry.path().join("x64").join("rc.exe");
            if rc.is_file() {
                candidates.push((key, rc));
            }
        }
    }

    candidates.sort_by_key(|(key, _)| *key);
    candidates.pop().map(|(_, path)| path)
}

fn find_rc() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("RC") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }

    // PATH 里有就直接用（MSVC 开发者命令行常见）
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }

    find_rc_in_kits()
}

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/sdf-mask-tool.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_path = manifest_dir
        .join("assets")
        .join("icon")
        .join("sdf-mask-tool.ico");
    if !ico_path.is_file() {
        println!(
            "cargo:warning=图标文件不存在，可执行文件不嵌入资源图标：{}",
            ico_path.display()
        );
        return;
    }

    let Some(rc) = find_rc() else {
        println!("cargo:warning=找不到 rc.exe（设置 RC 环境变量或安装 Windows SDK），跳过资源图标嵌入");
        return;
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("app-icon.rc");
    let res_file = out_dir.join("app-icon.res");

    // rc.exe 按本地 ANSI 代码页读 .rc，写进文件的中文路径会被截断成不存在的名字。
    // 因此 .rc 里只写图标的文件名，让 rc.exe 在图标所在目录里执行。
    let icon_dir = ico_path.parent().expect("图标路径缺少父目录");
    let icon_name = ico_path
        .file_name()
        .expect("图标路径缺少文件名")
        .to_string_lossy()
        .to_string();
    fs::write(&rc_file, format!("1 ICON \"{icon_name}\"\n")).expect("写入 .rc 失败");

    let output = Command::new(&rc)
        .current_dir(icon_dir)
        .arg("/nologo")
        .arg(format!("/fo{}", res_file.display()))
        .arg(&rc_file)
        .output();

    match output {
        Ok(out) if out.status.success() && res_file.is_file() => {
            println!("cargo:rustc-link-arg={}", res_file.display());
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            println!(
                "cargo:warning=rc.exe 编译图标资源失败（{}），可执行文件不嵌入资源图标：{stderr}",
                out.status
            );
        }
        Err(e) => {
            println!("cargo:warning=无法执行 rc.exe（{e}），可执行文件不嵌入资源图标");
        }
    }
}
