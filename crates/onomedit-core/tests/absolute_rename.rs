#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::{Path, PathBuf};

use onomedit_core::journal::RenameLogger;
use onomedit_core::pipeline::{PipelineError, PipelineOutcome, RenamePipeline, restore};
use onomedit_core::rules::Rule;
use onomedit_core::{Config, PathType};

fn pipeline() -> RenamePipeline {
    RenamePipeline::new(Config {
        path_type: PathType::Full,
        subdirs_depth: 0,
        ..Config::default()
    })
    .with_absolute(true)
}

fn strings(paths: &[&Path]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_str().unwrap().into())
        .collect()
}

fn edit(
    pipeline: &RenamePipeline,
    sources: &[&Path],
    targets: &[&Path],
    dry_run: bool,
    logger: &RenameLogger,
) -> Result<PipelineOutcome, PipelineError> {
    let session = pipeline.prepare(&strings(sources)).unwrap();
    let edit_file = session.edit_path().to_owned();
    fs::write(&edit_file, strings(targets).join("\r\n") + "\r\n").unwrap();
    let result = pipeline.complete(session, dry_run, Some(logger.clone()));
    assert!(!edit_file.exists());
    result
}

#[test]
fn absolute_preview_then_move_and_restore_with_collision() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("原 文件.txt");
    let other = dir.path().join("other.txt");
    let target = dir.path().join("new/nested/档案.txt");
    let occupied = dir.path().join("occupied.txt");
    let numbered = dir.path().join("occupied (1).txt");
    fs::write(&source, "first").unwrap();
    fs::write(&other, "second").unwrap();
    fs::write(&occupied, "existing").unwrap();
    let logger = RenameLogger::new(dir.path().join("log"));
    logger.begin_session();
    logger.record(Path::new("before"), Path::new("after"));
    let previous = fs::read(&logger.last_path).unwrap();
    let pipeline = pipeline();

    let preview = edit(
        &pipeline,
        &[&source, &other],
        &[&target, &occupied],
        true,
        &logger,
    )
    .unwrap();
    assert_eq!(preview.pairs[0].old, source);
    assert_eq!(preview.pairs[0].requested_new, target);
    assert_eq!(preview.pairs[1].requested_new, occupied);
    assert!(!target.parent().unwrap().exists());
    assert_eq!(fs::read(&logger.last_path).unwrap(), previous);

    let outcome = edit(
        &pipeline,
        &[&source, &other],
        &[&target, &occupied],
        false,
        &logger,
    )
    .unwrap();
    assert_eq!(
        outcome.result.success,
        [
            (source.clone(), target.clone()),
            (other.clone(), numbered.clone())
        ],
        "{:?}",
        outcome.result.failed
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "first");
    assert_eq!(fs::read_to_string(&occupied).unwrap(), "existing");
    assert_eq!(fs::read_to_string(&numbered).unwrap(), "second");
    assert_eq!(logger.read_last(), outcome.result.success);
    assert_eq!(restore(&logger, false, None).unwrap().success.len(), 2);
    assert_eq!(fs::read_to_string(&source).unwrap(), "first");
    assert_eq!(fs::read_to_string(&other).unwrap(), "second");
}

#[test]
fn absolute_preflight_errors_preserve_previous_session_and_batch() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    let requested = dir.path().join("absent/requested.txt");
    let logger = RenameLogger::new(dir.path().join("log"));
    logger.begin_session();
    logger.record(Path::new("before"), Path::new("after"));
    let previous = fs::read(&logger.last_path).unwrap();
    fs::write(&first, "first").unwrap();
    fs::write(&second, "second").unwrap();
    let targets = [
        PathBuf::from("relative.txt"),
        dir.path().join("absent/./requested.txt"),
        requested.join("inside.txt"),
        logger.last_path.clone(),
        dir.path().join("missing/../unsafe.txt"),
        dir.path().join("bad\u{2028}name.txt"),
        dir.path().join("bad<-->name.txt"),
        dir.path().join("first.txt/not-a-directory.txt"),
    ];

    for dry_run in [true, false] {
        for invalid in &targets {
            let result = edit(
                &pipeline(),
                &[&first, &second],
                &[&requested, invalid],
                dry_run,
                &logger,
            );
            assert!(result.is_err(), "{invalid:?}");
            assert!(result.unwrap_err().to_string().contains("行"));
            assert_eq!(fs::read(&logger.last_path).unwrap(), previous);
            assert!(!requested.parent().unwrap().exists());
            assert_eq!(fs::read_to_string(&first).unwrap(), "first");
            assert_eq!(fs::read_to_string(&second).unwrap(), "second");
        }
    }
}

#[test]
fn absolute_swap_and_skipped_items_reserve_their_paths() {
    let dir = tempfile::tempdir().unwrap();
    let [a, b, c, d, e, reserved, numbered] =
        ["a", "b", "c", "d", "e", "c (1)", "c (2)"].map(|name| dir.path().join(name));
    for path in [&a, &b, &c, &d, &e] {
        fs::write(path, path.file_name().unwrap().to_str().unwrap()).unwrap();
    }
    let logger = RenameLogger::new(dir.path().join("log"));
    let outcome = edit(
        &pipeline(),
        &[&a, &b, &c, &d, &e],
        &[&b, &a, &c, &c, &reserved],
        false,
        &logger,
    )
    .unwrap();
    assert_eq!(outcome.result.skipped, std::slice::from_ref(&c));
    assert!(
        outcome.result.failed.is_empty(),
        "{:?}",
        outcome.result.failed
    );
    assert_eq!(fs::read_to_string(&a).unwrap(), "b");
    assert_eq!(fs::read_to_string(&b).unwrap(), "a");
    assert_eq!(fs::read_to_string(&c).unwrap(), "c");
    assert_eq!(fs::read_to_string(&numbered).unwrap(), "d");
    assert_eq!(fs::read_to_string(&reserved).unwrap(), "e");
    assert!(
        !fs::read_to_string(&logger.last_path)
            .unwrap()
            .contains("__onomedit_tmp_")
    );
    assert!(restore(&logger, false, None).unwrap().failed.is_empty());
    assert_eq!(fs::read_to_string(&a).unwrap(), "a");
    assert_eq!(fs::read_to_string(&b).unwrap(), "b");
    assert_eq!(fs::read_to_string(&d).unwrap(), "d");
    assert_eq!(fs::read_to_string(&e).unwrap(), "e");
}

#[test]
fn absolute_path_rules_cannot_inject_directories_with_name_scope() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("first.txt");
    fs::write(&source, "first").unwrap();
    let mut pipeline = pipeline();
    pipeline.config.open_editor = false;
    pipeline.config.apply_rules = true;
    pipeline.config.enable_auto_rules = true;
    pipeline.config.temp_dir = dir
        .path()
        .join("unavailable-edit-dir")
        .to_str()
        .unwrap()
        .into();
    pipeline.config.auto_rules = vec![Rule {
        scope: PathType::Stem,
        find: "first".into(),
        replace: "nested/other".into(),
        ..Rule::default()
    }];
    let paths = strings(&[&source]);
    let result = pipeline.run_direct(&paths, true, None).unwrap_err();
    assert!(result.to_string().contains("名称段包含路径分隔符"));
    pipeline.config.auto_rules[0].scope = PathType::Full;
    // Full-path rules can move the item and --no-editor requires no temporary directory.
    let result = pipeline.run_direct(&paths, false, None).unwrap();
    assert_eq!(
        result.result.success,
        [(source, dir.path().join("nested/other.txt"))]
    );
    assert!(result.temp_path.as_os_str().is_empty());
}

#[test]
fn absolute_source_replaced_during_edit_aborts_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.txt");
    let target = dir.path().join("target.txt");
    fs::write(&source, "original").unwrap();
    let pipeline = pipeline();
    let session = pipeline.prepare(&strings(&[&source])).unwrap();
    let edit_path = session.edit_path().to_owned();
    fs::write(&edit_path, target.to_str().unwrap()).unwrap();
    fs::rename(&source, dir.path().join("saved.txt")).unwrap();
    fs::write(&source, "replacement").unwrap();
    let logger = RenameLogger::new(dir.path().join("log"));
    let error = pipeline
        .complete(session, false, Some(logger.clone()))
        .unwrap_err();
    assert!(error.to_string().contains("源目录项已变化"));
    assert!(!edit_path.exists());
    assert!(!target.exists());
    assert!(!logger.log_dir.exists());
    assert_eq!(fs::read_to_string(&source).unwrap(), "replacement");
}

#[test]
fn absolute_case_only_move_and_restore_leave_hard_links_intact() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("lower.txt");
    let target = dir.path().join("LOWER.txt");
    let linked = dir.path().join("another.txt");
    fs::write(&source, "payload").unwrap();
    fs::hard_link(&source, &linked).unwrap();
    let logger = RenameLogger::new(dir.path().join("log"));
    let outcome = edit(&pipeline(), &[&source], &[&target], false, &logger).unwrap();
    assert_eq!(
        outcome.result.success,
        [(source.clone(), target.clone())],
        "{:?}",
        outcome.result.failed
    );
    let names = || {
        fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    };
    assert!(names().contains(&target.file_name().unwrap().to_owned()));
    assert!(!names().contains(&source.file_name().unwrap().to_owned()));
    assert!(restore(&logger, false, None).unwrap().failed.is_empty());
    assert!(names().contains(&source.file_name().unwrap().to_owned()));
    assert_eq!(fs::read_to_string(&linked).unwrap(), "payload");

    let alternative = if target.exists() { &target } else { &source };
    let outcome = edit(&pipeline(), &[alternative], &[&linked], false, &logger).unwrap();
    let numbered = linked.with_file_name("another (1).txt");
    assert_eq!(outcome.result.success, [(alternative.to_owned(), numbered)]);
    assert_eq!(fs::read_to_string(&linked).unwrap(), "payload");
}

#[test]
fn absolute_protects_log_file_with_an_external_hard_link() {
    let dir = tempfile::tempdir().unwrap();
    let logger = RenameLogger::new(dir.path().join("log"));
    logger.begin_session();
    let source = dir.path().join("source.txt");
    fs::write(&source, "payload").unwrap();
    fs::remove_file(&logger.last_path).unwrap();
    fs::hard_link(&source, &logger.last_path).unwrap();
    let target = dir.path().join("target.txt");
    let error = edit(&pipeline(), &[&source], &[&target], false, &logger).unwrap_err();
    assert!(error.to_string().contains("活动日志目录冲突"));
    assert!(!target.exists());
    assert_eq!(fs::read_to_string(&source).unwrap(), "payload");

    let other = dir.path().join("other.txt");
    fs::write(&other, "other file").unwrap();
    assert!(edit(&pipeline(), &[&other], &[&target], false, &logger).is_err());
    assert_eq!(fs::read_to_string(&source).unwrap(), "payload");
    assert_eq!(fs::read_to_string(&other).unwrap(), "other file");
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn absolute_parent_aliases_preserve_entries_and_detect_dependencies() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    let alias = dir.path().join("alias");
    let a = real.join("a");
    let b = real.join("b");
    let logger = RenameLogger::new(dir.path().join("log"));
    fs::create_dir(&real).unwrap();
    fs::write(&a, "a").unwrap();
    fs::write(&b, "b").unwrap();
    symlink(&real, &alias).unwrap();

    let pipeline = pipeline();
    let session = pipeline.prepare(&strings(&[&a, &alias.join("a")])).unwrap();
    assert_eq!(session.items.len(), 1);
    let unchanged = edit(&pipeline, &[&a], &[&alias.join("a")], false, &logger).unwrap();
    assert_eq!(unchanged.result.skipped, std::slice::from_ref(&a));
    let conflict = edit(
        &pipeline,
        &[&real, &b],
        &[&dir.path().join("moved"), &alias.join("c")],
        true,
        &logger,
    )
    .unwrap_err();
    assert!(conflict.to_string().contains("目录依赖"));

    let occupied = real.join("dangling");
    symlink(real.join("missing"), &occupied).unwrap();
    let result = edit(&pipeline, &[&a], &[&alias.join("dangling")], false, &logger).unwrap();
    assert_eq!(result.result.success, [(a, alias.join("dangling (1)"))]);
    assert_eq!(fs::read_to_string(real.join("dangling (1)")).unwrap(), "a");
    assert!(occupied.is_symlink());
}

#[cfg(target_os = "linux")]
#[test]
fn absolute_preflight_rejects_cross_filesystem_moves() {
    use std::os::unix::fs::MetadataExt;

    let Ok(other_fs) = tempfile::tempdir_in("/dev/shm") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    if fs::metadata(dir.path()).unwrap().dev() == fs::metadata(other_fs.path()).unwrap().dev() {
        return;
    }
    let source = dir.path().join("source");
    fs::write(&source, "payload").unwrap();
    let target = other_fs.path().join("target");
    let logger = RenameLogger::new(dir.path().join("log"));
    let error = edit(&pipeline(), &[&source], &[&target], false, &logger).unwrap_err();
    assert!(error.to_string().contains("跨卷移动"));
    assert!(!target.exists());
    assert!(!logger.log_dir.exists());
    assert_eq!(fs::read_to_string(&source).unwrap(), "payload");
}
