// 打包时把 AI 配置（已混淆）编译进 exe，发布目录里不再落地明文密钥。
// 构建前把密钥做 XOR + Base64 预处理，再以环境变量传进来；不设这些变量时
// 生成的是空常量，运行期自动回落到 .env 里的配置。
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    for name in ["QINGFENG_AI_KEYS_OBF", "QINGFENG_AI_BASE_URL", "QINGFENG_AI_MODEL"] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let read = |name: &str| env::var(name).unwrap_or_default().trim().to_string();
    let source = format!(
        "pub const BUILTIN_KEYS_OBF: &str = {:?};\n\
         pub const BUILTIN_BASE_URL_OBF: &str = {:?};\n\
         pub const BUILTIN_MODEL_OBF: &str = {:?};\n",
        read("QINGFENG_AI_KEYS_OBF"),
        read("QINGFENG_AI_BASE_URL"),
        read("QINGFENG_AI_MODEL"),
    );

    let target = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR")).join("builtin_keys.rs");
    fs::write(&target, source).expect("写入内置密钥常量失败");

    embed_icon();
}

/// 把 assets/icon.ico 编进 exe，资源管理器和任务管理器都会显示这个图标。
/// 找不到 Windows SDK 的 rc.exe 时静默跳过，不影响编译。
fn embed_icon() {
    // 只有 Windows 目标才有资源段；交叉编译 Android/macOS 时 rc.exe 依然存在，
    // 但把 .res 递给 Android 链接器会直接失败，所以按目标平台短路。
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("缺少 CARGO_MANIFEST_DIR"));
    let icon = manifest.join("assets").join("icon.ico");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if !icon.exists() {
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=未找到 rc.exe，跳过 exe 图标");
        return;
    };

    let out = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR"));
    let script = out.join("app.rc");
    let resource = out.join("app.res");
    fs::write(
        &script,
        format!("1 ICON \"{}\"\n", icon.display().to_string().replace('\\', "\\\\")),
    )
    .expect("写入 rc 脚本失败");

    let status = Command::new(rc).arg("/nologo").arg("/fo").arg(&resource).arg(&script).status();
    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg={}", resource.display());
        }
        // 资源链接失败不应该挡住编译，顶多没有 exe 图标。
        _ => println!("cargo:warning=exe 图标编译失败，继续构建"),
    }
}

/// 在 Windows SDK 的 bin 目录里找 rc.exe（取版本号最大的那个）。
fn find_rc() -> Option<PathBuf> {
    let roots = [
        PathBuf::from("C:/Program Files (x86)/Windows Kits/10/bin"),
        PathBuf::from("C:/Program Files/Windows Kits/10/bin"),
    ];
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let rc = entry.path().join("x64").join("rc.exe");
            if rc.exists() {
                candidates.push(rc);
            }
        }
    }
    candidates.sort();
    candidates.pop()
}
