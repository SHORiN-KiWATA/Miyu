//! 包管理器:清单/来源解析、本地包的装卸升、冲突、tar 路径逃逸。

use super::*;
use crate::config::AppConfig;

fn test_paths(root: &Path) -> MiyuPaths {
    MiyuPaths {
        root_dir: root.to_path_buf(),
        config_dir: root.join("config"),
        config_file: root.join("config/config.jsonc"),
        skills_dir: root.join("extensions/skills"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        pictures_dir: root.join("home/shorin/pictures"),
        fish_hook_file: root.join("fish/miyu.fish"),
        bash_hook_file: root.join("config/shell/bash-hook.sh"),
        zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
        scripts_dir: root.join("extensions/scripts"),
        system_scripts_dir: PathBuf::new(),
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn extension_package(root: &Path, name: &str, version: &str) -> PathBuf {
    let pkg = root.join(format!("pkg-{name}"));
    write(
        &pkg.join(MANIFEST_FILE),
        &format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"demo\"\n"),
    );
    write(
        &pkg.join("scripts/hello.py"),
        &format!("#!/usr/bin/env python3\n# Description: Say hello v{version}.\nprint('hi')\n"),
    );
    write(
        &pkg.join("skills/greeter/SKILL.md"),
        "---\nname: greeter\ndescription: Greets people warmly.\n---\n# Greeter\n",
    );
    write(&pkg.join("skills/greeter/notes/extra.md"), "extra");
    pkg
}

#[test]
fn manifest_parses_and_validates_names_and_requirements() {
    let manifest = PackageManifest::parse(
        "[package]\nname = \"bangumi-tools\"\nversion = \"1.2.0\"\nrequires-miyu = \">=0.1.0\"\n",
    )
    .unwrap();
    assert_eq!(manifest.package.name, "bangumi-tools");
    assert_eq!(manifest.package.kind, PackageKind::Extension);
    manifest.check_requirement().unwrap();

    assert!(PackageManifest::parse("[package]\nname = \"Bad Name\"\n").is_err());
    assert!(
        PackageManifest::parse("[package]\nname = \"ok\"\nrequires-miyu = \"banana\"\n").is_err()
    );
    let future =
        PackageManifest::parse("[package]\nname = \"ok\"\nrequires-miyu = \">=999.0.0\"\n")
            .unwrap();
    assert!(future.check_requirement().is_err());
    assert!(PackageManifest::parse("[package]\nname = \"ok\"\nbogus = 1\n").is_err());
    assert_eq!(parse_version("0.5.0-2").unwrap(), (0, 5, 0));
    assert_eq!(parse_requirement("^0.5").unwrap(), (0, 5, 0));
}

#[test]
fn package_specs_resolve_local_github_and_slug_forms() {
    let temp = tempfile::tempdir().unwrap();
    let pkg = temp.path().join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    let local = PackageSource::parse_spec(&pkg.display().to_string(), None)
        .unwrap()
        .unwrap();
    assert!(matches!(local, PackageSource::Local(_)));
    assert_eq!(
        PackageSource::parse_spec("shorin/miyu-bangumi@v1", None)
            .unwrap()
            .unwrap(),
        PackageSource::GitHub {
            owner: "shorin".into(),
            repo: "miyu-bangumi".into(),
            reference: Some("v1".into())
        }
    );
    assert_eq!(
        PackageSource::parse_spec("https://github.com/shorin/miyu-bangumi/tree/main", None)
            .unwrap()
            .unwrap(),
        PackageSource::GitHub {
            owner: "shorin".into(),
            repo: "miyu-bangumi".into(),
            reference: Some("main".into())
        }
    );
    // 裸包名要查索引
    assert!(PackageSource::parse_spec("bangumi", None)
        .unwrap()
        .is_none());
    assert!(PackageSource::parse_spec("../escape", None).is_err());
    assert!(validate_repo_slug("a/../b").is_err());
}

#[test]
fn extension_package_installs_removes_and_upgrades() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".miyu");
    let paths = test_paths(&root);
    fs::create_dir_all(&root).unwrap();
    let config = AppConfig::default();
    let pkg = extension_package(temp.path(), "demo", "1.0.0");
    let source = PackageSource::Local(pkg.clone());

    let plan = plan_install(&config, &paths, &pkg).unwrap();
    assert_eq!(plan.files.len(), 3);
    let installed = install(&paths, &plan, &source, None, false).unwrap();
    assert_eq!(installed.files.len(), 3);
    let script = root.join("extensions/scripts/hello.py");
    assert!(script.is_file());
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o111,
        0o111
    );
    assert!(root.join("extensions/skills/greeter/SKILL.md").is_file());
    assert!(root
        .join("extensions/skills/greeter/notes/extra.md")
        .is_file());
    let lock = load_lock(&paths).unwrap();
    assert_eq!(lock.packages["demo"].version, "1.0.0");

    // 同内容再装 = 已是最新
    let plan_again = plan_install(&config, &paths, &pkg).unwrap();
    assert!(is_up_to_date(&lock.packages["demo"], &plan_again, None).unwrap());

    // 新版本:重装,旧文件先卸
    let pkg2 = extension_package(temp.path(), "demo", "2.0.0");
    let plan2 = plan_install(&config, &paths, &pkg2).unwrap();
    assert!(!is_up_to_date(&lock.packages["demo"], &plan2, None).unwrap());
    install(&paths, &plan2, &PackageSource::Local(pkg2), None, false).unwrap();
    assert!(fs::read_to_string(&script).unwrap().contains("v2.0.0"));
    assert_eq!(load_lock(&paths).unwrap().packages["demo"].version, "2.0.0");

    // 卸载:文件与空目录一起没了,extensions 根留着
    remove(&paths, "demo").unwrap();
    assert!(!script.exists());
    assert!(!root.join("extensions/skills/greeter").exists());
    assert!(root.join("extensions/scripts").is_dir());
    assert!(load_lock(&paths).unwrap().packages.is_empty());
    assert!(remove(&paths, "demo").is_err());
}

#[test]
fn install_refuses_files_owned_by_others_unless_forced() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".miyu");
    let paths = test_paths(&root);
    fs::create_dir_all(&root).unwrap();
    let config = AppConfig::default();
    let a = extension_package(temp.path(), "aaa", "1.0.0");
    let plan_a = plan_install(&config, &paths, &a).unwrap();
    install(&paths, &plan_a, &PackageSource::Local(a), None, false).unwrap();
    // 另一个包想装同名脚本
    let b = extension_package(temp.path(), "bbb", "1.0.0");
    let plan_b = plan_install(&config, &paths, &b).unwrap();
    let error = install(
        &paths,
        &plan_b,
        &PackageSource::Local(b.clone()),
        None,
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("owned by package aaa"),
        "{error:#}"
    );
    // 用户手放的文件也挡,--force 放行
    remove(&paths, "aaa").unwrap();
    write(&root.join("extensions/scripts/hello.py"), "mine");
    assert!(install(
        &paths,
        &plan_b,
        &PackageSource::Local(b.clone()),
        None,
        false
    )
    .is_err());
    install(&paths, &plan_b, &PackageSource::Local(b), None, true).unwrap();
    assert!(fs::read_to_string(root.join("extensions/scripts/hello.py"))
        .unwrap()
        .contains("v1.0.0"));
}

#[test]
fn persona_package_lands_prompt_manifest_assets_and_scoped_extensions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".miyu");
    let paths = test_paths(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".home-layout-v1"), "shorin\n").unwrap();
    let config = AppConfig::default();
    let pkg = temp.path().join("pkg-sakura");
    write(
        &pkg.join(MANIFEST_FILE),
        "[package]\nname = \"sakura\"\nversion = \"0.1.0\"\nkind = \"persona\"\n",
    );
    write(&pkg.join("persona/persona.md"), "You are Sakura.");
    write(
        &pkg.join("persona/persona.json"),
        "{\"avatar_path\": \"sakura/avatar.png\"}",
    );
    write(
        &pkg.join("persona/persona.toml"),
        "[subsystems]\nmemory = true\n\n[plugins]\nenabled = [\"files\"]\n",
    );
    write(&pkg.join("persona/assets/avatar.png"), "png");
    write(
        &pkg.join("scripts/fortune.py"),
        "#!/usr/bin/env python3\n# Description: Sakura's fortune.\nprint('x')\n",
    );
    let plan = plan_install(&config, &paths, &pkg).unwrap();
    assert_eq!(plan.persona_scope.as_deref(), Some("sakura-md"));
    install(&paths, &plan, &PackageSource::Local(pkg), None, false).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("data/prompts/sakura.md")).unwrap(),
        "You are Sakura."
    );
    assert!(root.join("data/prompts/sakura.json").is_file());
    assert!(root.join("personas/sakura-md/persona.toml").is_file());
    assert!(root
        .join("data/persona-avatars/sakura/avatar.png")
        .is_file());
    assert!(root
        .join("extensions/scripts/personas/sakura-md/fortune.py")
        .is_file());
    remove(&paths, "sakura").unwrap();
    assert!(!root.join("data/prompts/sakura.md").exists());
    assert!(!root.join("personas/sakura-md").exists());
}

#[test]
fn invalid_packages_are_rejected_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".miyu");
    let paths = test_paths(&root);
    fs::create_dir_all(&root).unwrap();
    let config = AppConfig::default();
    // 技能没有 SKILL.md
    let pkg = temp.path().join("pkg-bad");
    write(&pkg.join(MANIFEST_FILE), "[package]\nname = \"bad\"\n");
    write(&pkg.join("skills/thing/README.md"), "no skill file");
    assert!(plan_install(&config, &paths, &pkg).is_err());
    // 什么都不装
    let empty = temp.path().join("pkg-empty");
    write(&empty.join(MANIFEST_FILE), "[package]\nname = \"empty\"\n");
    assert!(plan_install(&config, &paths, &empty).is_err());
    // 人格包缺 persona.md
    let persona = temp.path().join("pkg-p");
    write(
        &persona.join(MANIFEST_FILE),
        "[package]\nname = \"p-one\"\nkind = \"persona\"\n",
    );
    assert!(plan_install(&config, &paths, &persona).is_err());
    assert!(!root.join("extensions").exists());
}

#[test]
fn tarballs_that_escape_the_package_are_rejected() {
    let mut buffer = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buffer, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let data = b"[package]\nname = \"x-y\"\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "repo-main/miyu-package.toml", &data[..])
            .unwrap();
        // tar crate 自己不肯写 `..` 路径,直接填 header 的 name 字段绕过它——
        // 恶意归档就是这么来的。
        let mut evil = tar::Header::new_gnu();
        {
            let name = b"repo-main/../../escape";
            evil.as_old_mut().name[..name.len()].copy_from_slice(name);
        }
        evil.set_size(1);
        evil.set_mode(0o644);
        evil.set_cksum();
        builder.append(&evil, &b"x"[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let temp = tempfile::tempdir().unwrap();
    let error = extract_tarball(&buffer, temp.path()).unwrap_err();
    assert!(error.to_string().contains("escapes"), "{error:#}");
}

#[test]
fn taps_always_include_the_official_one() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".miyu");
    let paths = test_paths(&root);
    fs::create_dir_all(&root).unwrap();
    assert_eq!(load_taps(&paths).unwrap(), vec![OFFICIAL_TAP.to_string()]);
    save_taps(&paths, &["me/tap".to_string()]).unwrap();
    assert_eq!(
        load_taps(&paths).unwrap(),
        vec![OFFICIAL_TAP.to_string(), "me/tap".to_string()]
    );
}
