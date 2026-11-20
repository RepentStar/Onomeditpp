use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::absolute_path::{self, DirectoryIdentity, Parent, Source, key};
use crate::journal::{CheckedSession, RenameLogger};
use crate::native_move::{self, Identity, entry_identity, occupied, rename_no_replace};
use crate::pipeline::{PipelineError, RenamePair, RenameResult};
use crate::safe_name::numbered_path;

struct Move {
    row: usize,
    source: Source,
    target: PathBuf,
    parent: Parent,
    occupant: Option<Identity>,
    skipped: bool,
    case_only: bool,
}

impl Move {
    fn target_key(&self) -> PathBuf {
        self.parent.child_key(&self.target)
    }

    fn describe(&self, error: impl std::fmt::Display) -> String {
        absolute_path::row_error(self.row, &self.source.path, &self.target, error)
    }
}

pub(crate) struct MovePlan {
    moves: Vec<Move>,
    reserved: HashSet<PathBuf>,
    logs: Option<(PathBuf, Parent)>,
}

impl MovePlan {
    pub fn build(
        sources: Vec<Source>,
        pairs: &[RenamePair],
        logger: Option<&RenameLogger>,
    ) -> Result<Self, PipelineError> {
        if sources.len() != pairs.len()
            || sources
                .iter()
                .zip(pairs)
                .any(|(source, pair)| source.path != pair.old)
        {
            return Err(PipelineError::Absolute("源目录项与编辑计划不匹配".into()));
        }
        let logs = logger
            .map(|logger| -> io::Result<_> {
                let path = absolute_path::lexical(&std::env::current_dir()?.join(&logger.log_dir));
                let parent = Parent::read(&path)?;
                Ok((path, parent))
            })
            .transpose()?;
        let mut log_entries = Vec::new();
        if let Some((_, parent)) = &logs {
            if parent.missing.is_empty() {
                let logger = RenameLogger::new(&parent.resolved);
                for path in [&logger.last_path, &logger.history_path, &logger.error_path] {
                    if let Some(entry) = entry_identity(path)? {
                        log_entries.push(entry);
                        if native_move::is_redirect(&fs::symlink_metadata(path)?) {
                            match native_move::identity(path, true) {
                                Ok(referent) => log_entries.push(referent),
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                    }
                }
            }
        }

        let mut moves = Vec::<Move>::with_capacity(pairs.len());
        let mut goals = HashMap::<PathBuf, usize>::new();
        let mut ancestors = HashSet::new();
        let mut reserved = HashSet::new();
        for (index, (source, pair)) in sources.into_iter().zip(pairs).enumerate() {
            let movement =
                Self::check_move(index + 1, source, &pair.requested_new).map_err(|error| {
                    PipelineError::Absolute(absolute_path::row_error(
                        index + 1,
                        &pair.old,
                        &pair.requested_new,
                        error,
                    ))
                })?;
            let source_key = movement.source.key();
            let target_key = movement.target_key();
            let source_parent = parent_of(&movement.source.path);
            let target_parent = parent_of(&movement.target);
            if let Some((log_path, log_parent)) = &logs {
                let log_key = key(&log_parent.resolved);
                if log_parent
                    .ancestors(log_path)
                    .any(|ancestor| ancestor == source_key || ancestor == target_key)
                    || movement
                        .source
                        .parent
                        .ancestors(source_parent)
                        .any(|ancestor| ancestor == log_key)
                    || movement
                        .parent
                        .ancestors(target_parent)
                        .any(|ancestor| ancestor == log_key)
                    || log_entries.contains(&movement.source.identity)
                    || movement
                        .occupant
                        .is_some_and(|entry| log_entries.contains(&entry))
                {
                    return Err(PipelineError::Absolute(
                        movement.describe("路径与活动日志目录冲突"),
                    ));
                }
            }
            ancestors.extend(movement.source.parent.ancestors(source_parent));
            if !movement.skipped {
                if let Some(previous) = goals.insert(target_key.clone(), index) {
                    let reason = "检测到目标重名";
                    return Err(PipelineError::Absolute(format!(
                        "{}\n{}",
                        moves[previous].describe(reason),
                        movement.describe(reason),
                    )));
                }
                ancestors.extend(movement.parent.ancestors(target_parent));
            }
            reserved.extend([source_key, target_key]);
            moves.push(movement);
        }
        for movement in moves.iter().filter(|movement| !movement.skipped) {
            if ancestors.contains(&movement.source.key())
                || ancestors.contains(&movement.target_key())
            {
                return Err(PipelineError::Absolute(
                    movement.describe("存在目录依赖，请拆成多次执行"),
                ));
            }
        }
        reserved.extend(ancestors);
        Ok(Self {
            moves,
            reserved,
            logs,
        })
    }

    fn check_move(row: usize, source: Source, target: &Path) -> io::Result<Move> {
        let target = absolute_path::target(target, true)?;
        if Parent::read(parent_of(&source.path))? != source.parent {
            return Err(io::Error::other("源父目录已变化"));
        }
        source.check_entry(&source.path)?;
        let parent = Parent::read(parent_of(&target))?;
        let occupant = if parent.missing.is_empty() {
            entry_identity(&target)?
        } else {
            None
        };
        let (skipped, case_only) = same_entry(&source, &target, &parent, occupant)?;
        if !skipped {
            if source.parent.device() != parent.device() {
                return Err(io::Error::other("跨卷移动需要在同一卷内执行"));
            }
            absolute_path::target(&physical_entry(&source.parent, &source.path), true)?;
            absolute_path::target(&physical_entry(&parent, &target), true)?;
        }
        Ok(Move {
            row,
            source,
            target,
            parent,
            occupant,
            skipped,
            case_only,
        })
    }

    pub fn execute(self, reset_log: bool) -> Result<RenameResult, PipelineError> {
        let Self {
            moves,
            reserved,
            logs,
        } = self;
        if moves.iter().any(|movement| !movement.skipped) {
            native_move::check_support()?;
        }
        let mut runner = Runner {
            reserved,
            created: HashMap::new(),
        };
        let mut log = logs
            .map(|(path, parent)| {
                runner.check_parent(&path, &parent, true)?;
                RenameLogger::new(parent.resolved).begin_checked_session(reset_log)
            })
            .transpose()?;
        Ok(runner.run(moves, log.as_mut()))
    }
}

fn parent_of(path: &Path) -> &Path {
    path.parent().expect("validated absolute path")
}

fn physical_entry(parent: &Parent, path: &Path) -> PathBuf {
    parent
        .resolved
        .join(path.file_name().expect("validated name"))
}

fn same_entry(
    source: &Source,
    target: &Path,
    parent: &Parent,
    occupant: Option<Identity>,
) -> io::Result<(bool, bool)> {
    if key(&source.parent.resolved) != key(&parent.resolved) || occupant != Some(source.identity) {
        return Ok((false, false));
    }
    let old = source.path.file_name().expect("source name");
    let new = target.file_name().expect("target name");
    if old == new {
        return Ok((true, false));
    }
    if old.to_string_lossy().to_lowercase() != new.to_string_lossy().to_lowercase() {
        return Ok((false, false));
    }
    let mut old_is_name = false;
    let mut new_is_name = false;
    for entry in fs::read_dir(&parent.resolved)? {
        let name = entry?.file_name();
        old_is_name |= name == old;
        new_is_name |= name == new;
    }
    // Hard links appear as separate names. An alias for an existing spelling is a no-op.
    // Only a requested spelling absent from the directory needs a temporary name.
    let same = !old_is_name || !new_is_name;
    Ok((same && new_is_name, same && !new_is_name))
}

struct Runner {
    reserved: HashSet<PathBuf>,
    created: HashMap<PathBuf, DirectoryIdentity>,
}

impl Runner {
    fn run(&mut self, moves: Vec<Move>, mut log: Option<&mut CheckedSession>) -> RenameResult {
        let mut result = RenameResult::default();
        let moving: HashMap<_, _> = moves
            .iter()
            .enumerate()
            .filter(|(_, movement)| !movement.skipped)
            .map(|(index, movement)| (movement.source.key(), index))
            .collect();
        let dependencies: Vec<_> = moves
            .iter()
            .enumerate()
            .map(|(index, movement)| {
                if movement.skipped {
                    return None;
                }
                let occupant = if movement.case_only {
                    Some(index)
                } else {
                    moving.get(&movement.target_key()).copied()
                };
                occupant.filter(|&index| movement.occupant == Some(moves[index].source.identity))
            })
            .collect();
        let stage_indices: HashSet<_> = dependencies.iter().flatten().copied().collect();
        let mut staged = HashMap::new();
        let mut stage_failures = HashSet::new();
        for (index, movement) in moves.iter().enumerate() {
            if stage_indices.contains(&index) {
                match self.stage(movement) {
                    Ok(path) => {
                        staged.insert(index, path);
                    }
                    Err(error) => {
                        stage_failures.insert(index);
                        report_failure(
                            &mut result,
                            log.as_deref_mut(),
                            movement,
                            error.to_string(),
                        );
                    }
                }
            }
        }

        for (index, movement) in moves.iter().enumerate() {
            if movement.skipped {
                result.skipped.push(movement.source.path.clone());
                continue;
            }
            if stage_failures.contains(&index) {
                continue;
            }
            let temp = staged.remove(&index);
            let source_path = temp
                .clone()
                .unwrap_or_else(|| physical_entry(&movement.source.parent, &movement.source.path));
            let dependency = dependencies[index];
            let operation = if dependency.is_some_and(|index| stage_failures.contains(&index)) {
                Err(io::Error::other("待移动的目标占用项暂存失败"))
            } else {
                self.place(movement, &source_path, dependency.is_some())
            };
            match operation {
                Ok(actual) => {
                    let recorded = log
                        .as_deref_mut()
                        .map_or(Ok(()), |log| log.record(&movement.source.path, &actual));
                    if let Err(error) = recorded {
                        report_failure(
                            &mut result,
                            log.as_deref_mut(),
                            movement,
                            format!(
                                "项目已移动至 \"{}\"，写入恢复日志失败: {error}",
                                actual.display()
                            ),
                        );
                    } else {
                        result.success.push((movement.source.path.clone(), actual));
                    }
                }
                Err(error) => {
                    let mut reason = if error.kind() == io::ErrorKind::CrossesDevices {
                        format!("跨卷移动需要在同一卷内执行 ({error})")
                    } else {
                        error.to_string()
                    };
                    if let Some(temp) = temp {
                        let restore = self.check_source(movement, &temp).and_then(|()| {
                            rename_no_replace(
                                &temp,
                                &physical_entry(&movement.source.parent, &movement.source.path),
                            )
                        });
                        if let Err(error) = restore {
                            reason.push_str(&format!(
                                "\n暂存恢复失败，恢复路径: \"{}\"；{error}",
                                temp.display()
                            ));
                        }
                    }
                    report_failure(&mut result, log.as_deref_mut(), movement, reason);
                }
            }
        }
        result
    }

    fn check_parent(&mut self, path: &Path, expected: &Parent, create: bool) -> io::Result<()> {
        loop {
            let current = Parent::read(path)?;
            let changed = || io::Error::other(format!("父目录已变化: {path:?}"));
            let Some(extra) = current.existing.strip_prefix(expected.existing.as_slice()) else {
                return Err(changed());
            };
            if key(&current.resolved) != key(&expected.resolved) {
                return Err(changed());
            }
            for (directory, identity) in extra {
                if self.created.get(&key(&absolute_path::physical(directory)?)) != Some(identity) {
                    return Err(changed());
                }
            }
            if !create || current.missing.is_empty() {
                return Ok(());
            }
            let (anchor, _) = current.existing.last().expect("parent root exists");
            let physical_anchor = absolute_path::physical(anchor)?;
            let missing = physical_anchor.join(&current.missing[0]);
            if self.created.contains_key(&key(&missing)) {
                return Err(changed());
            }
            fs::create_dir(&missing)?;
            let new = Parent::read(&missing)?;
            let metadata = fs::symlink_metadata(&missing)?;
            if native_move::is_redirect(&metadata)
                || !new.missing.is_empty()
                || key(&new.resolved) != key(&missing)
            {
                return Err(changed());
            }
            let (_, identity) = new.existing.last().expect("created directory");
            self.created.insert(key(&missing), *identity);
        }
    }

    fn check_source(&mut self, movement: &Move, actual: &Path) -> io::Result<()> {
        self.check_parent(
            parent_of(&movement.source.path),
            &movement.source.parent,
            false,
        )?;
        movement.source.check_entry(actual)
    }

    fn stage(&mut self, movement: &Move) -> io::Result<PathBuf> {
        self.check_parent(parent_of(&movement.target), &movement.parent, false)?;
        let source = physical_entry(&movement.source.parent, &movement.source.path);
        loop {
            let temporary =
                source.with_file_name(format!(".__onomedit_tmp_{}", uuid::Uuid::new_v4().simple()));
            let temporary = absolute_path::target(&temporary, true)?;
            let name = key(&temporary);
            if self.reserved.contains(&name) {
                continue;
            }
            self.check_source(movement, &source)?;
            match rename_no_replace(&source, &temporary) {
                Ok(()) => {
                    self.reserved.insert(name);
                    return Ok(temporary);
                }
                Err(error) if occupied(&error) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn place(
        &mut self,
        movement: &Move,
        source_path: &Path,
        has_dependency: bool,
    ) -> io::Result<PathBuf> {
        self.check_source(movement, source_path)?;
        let parent = parent_of(&movement.target);
        self.check_parent(parent, &movement.parent, true)?;
        for index in 0.. {
            let candidate = if index == 0 {
                movement.target.clone()
            } else {
                numbered_path(&movement.target, index)
            };
            let candidate = absolute_path::target(&candidate, true)?;
            let candidate_key = movement.parent.child_key(&candidate);
            if index != 0 && self.reserved.contains(&candidate_key) {
                continue;
            }
            let actual = physical_entry(&movement.parent, &candidate);
            let actual = absolute_path::target(&actual, true)?;
            self.check_source(movement, source_path)?;
            self.check_parent(parent, &movement.parent, false)?;
            if entry_identity(&actual)?.is_none() {
                match rename_no_replace(source_path, &actual) {
                    Ok(()) => {
                        self.reserved.insert(candidate_key);
                        return Ok(candidate);
                    }
                    Err(error)
                        if occupied(&error)
                            || entry_identity(&actual).is_ok_and(|entry| entry.is_some()) => {}
                    Err(error) => return Err(error),
                }
            }
            if has_dependency {
                return Err(io::Error::other("待移动的目标占用项已变化"));
            }
        }
        unreachable!()
    }
}

fn report_failure(
    result: &mut RenameResult,
    log: Option<&mut CheckedSession>,
    movement: &Move,
    mut reason: String,
) {
    if let Some(log) = log {
        if let Err(error) = log.record_error(&movement.describe(&reason)) {
            reason.push_str(&format!("\n写入错误日志失败: {error}"));
        }
    }
    result.failed.push((
        movement.source.path.clone(),
        movement.target.clone(),
        format!("第 {} 行: {reason}", movement.row),
    ));
}

#[cfg(all(test, any(windows, target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    fn plan(pairs: &[RenamePair]) -> MovePlan {
        let cwd = std::env::current_dir().unwrap();
        let sources = pairs
            .iter()
            .map(|pair| Source::read(&pair.old, &cwd).unwrap())
            .collect();
        MovePlan::build(sources, pairs, None).unwrap()
    }

    #[test]
    fn absolute_native_move_rejects_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let target = root.path().join("target.txt");
        fs::write(&source, "source").unwrap();
        fs::write(&target, "target").unwrap();
        assert!(rename_no_replace(&source, &target).is_err());
        assert_eq!(fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(fs::read_to_string(&target).unwrap(), "target");
    }

    #[test]
    fn absolute_move_numbers_late_collision_but_rejects_a_new_parent() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.txt");
        let second = root.path().join("second.txt");
        let target = root.path().join("target.txt");
        let parent = root.path().join("parent");
        fs::write(&source, "source").unwrap();
        fs::write(&second, "second").unwrap();
        let movement = plan(&[
            RenamePair::new(&source, &target),
            RenamePair::new(&second, parent.join("target.txt")),
        ]);
        fs::create_dir(&parent).unwrap();
        fs::write(&target, "late arrival").unwrap();
        let result = movement.execute(true).unwrap();
        assert_eq!(
            result.success,
            [(source, root.path().join("target (1).txt"))]
        );
        assert_eq!(result.failed.len(), 1);
        assert!(result.failed[0].2.contains("父目录已变化"));
        assert_eq!(fs::read_to_string(&second).unwrap(), "second");
        assert_eq!(fs::read_to_string(&target).unwrap(), "late arrival");
        assert!(!parent.join("target.txt").exists());
    }

    #[test]
    fn absolute_move_rolls_back_if_a_staged_dependency_changes() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let second = root.path().join("second.txt");
        fs::write(&first, "first").unwrap();
        fs::write(&second, "second").unwrap();
        let movement = plan(&[
            RenamePair::new(&first, &second),
            RenamePair::new(&second, &first),
        ]);
        fs::rename(&second, root.path().join("original-second.txt")).unwrap();
        fs::write(&second, "replacement").unwrap();
        let result = movement.execute(true).unwrap();
        assert_eq!(result.failed.len(), 2);
        assert!(result.success.is_empty());
        assert_eq!(fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(fs::read_to_string(&second).unwrap(), "replacement");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }
}
