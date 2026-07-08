//! Target-scoped projection state reconciliation.
//!
//! This module compares the durable source-of-truth state for a mount with
//! derived projection state such as virtual filesystem mutations and local
//! Markdown identity. It is deliberately connector-neutral: recoveries here
//! must be lossless state normalizations, while ambiguous cases remain visible
//! as conflicts for the normal push/pull review flow.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use locality_core::LocalityResult;
use locality_core::canonical::parse_canonical_markdown;
use locality_core::model::RemoteId;
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
};

use crate::file_provider;
use crate::virtual_fs::virtual_fs_content_path;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionStateReconcileReport {
    pub checked: usize,
    pub repaired: usize,
    pub conflicts: usize,
    pub diagnostics: Vec<ProjectionStateDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionStateDiagnostic {
    pub code: String,
    pub path: PathBuf,
    pub local_id: Option<String>,
    pub remote_id: Option<String>,
    pub message: String,
    pub repair: Option<ProjectionStateRepairKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionStateRepairKind {
    ClearRedundantPendingCreate,
    ClearOrphanPendingDelete,
    ClearStalePendingDelete,
    ClearRedundantPendingRename,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionStatePlan {
    checked: usize,
    diagnostics: Vec<ProjectionStateDiagnostic>,
    repairs: Vec<ProjectionStateRepair>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionStateRepair {
    mount_id: locality_core::model::MountId,
    local_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectionStateScope {
    mount: MountConfig,
    filter: ProjectionStateFilter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProjectionStateFilter {
    All,
    Exact(PathBuf),
    Subtree(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingCreateIdentity {
    Remote(RemoteId),
    MissingIdentity,
    Unreadable,
    InvalidCanonical,
    ConflictingSources(Vec<RemoteId>),
}

#[derive(Debug)]
struct ProjectionStateFacts {
    entities_by_id: BTreeMap<RemoteId, EntityRecord>,
    entities_by_path: BTreeMap<PathBuf, EntityRecord>,
    mutations: Vec<VirtualMutationRecord>,
    mutations_by_path: BTreeMap<PathBuf, Vec<VirtualMutationRecord>>,
    mutations_by_remote_id: BTreeMap<RemoteId, Vec<VirtualMutationRecord>>,
}

pub fn diagnose_projection_state_for_target<S>(
    store: &S,
    state_root: Option<&Path>,
    target: Option<&Path>,
) -> LocalityResult<ProjectionStateReconcileReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let plan = plan_projection_state_reconciliation(store, state_root, target)?;
    Ok(report_from_plan(&plan, 0))
}

pub fn reconcile_projection_state_for_target<S>(
    store: &mut S,
    state_root: Option<&Path>,
    target: Option<&Path>,
) -> LocalityResult<ProjectionStateReconcileReport>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let plan = plan_projection_state_reconciliation(store, state_root, target)?;
    let mut repaired = 0;
    for repair in &plan.repairs {
        store.delete_virtual_mutation(&repair.mount_id, &repair.local_id)?;
        repaired += 1;
    }
    Ok(report_from_plan(&plan, repaired))
}

pub fn redundant_pending_create_entity<S>(
    store: &S,
    state_root: Option<&Path>,
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
) -> LocalityResult<Option<EntityRecord>>
where
    S: EntityRepository,
{
    if mutation.mutation_kind != VirtualMutationKind::Create {
        return Ok(None);
    }
    let Some(entity) = store.find_entity_by_path(&mount.mount_id, &mutation.projected_path)? else {
        return Ok(None);
    };
    match pending_create_identity(mount, state_root, mutation)? {
        PendingCreateIdentity::Remote(remote_id) if remote_id == entity.remote_id => {
            Ok(Some(entity))
        }
        _ => Ok(None),
    }
}

fn plan_projection_state_reconciliation<S>(
    store: &S,
    state_root: Option<&Path>,
    target: Option<&Path>,
) -> LocalityResult<ProjectionStatePlan>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let scopes = projection_state_scopes(store, target)?;
    let mut plan = ProjectionStatePlan {
        checked: 0,
        diagnostics: Vec::new(),
        repairs: Vec::new(),
    };

    for scope in scopes {
        let facts = ProjectionStateFacts::load(store, &scope)?;
        plan.checked += facts.mutations.len();
        plan_multi_mutation_conflicts(&scope.mount, &facts, &mut plan);
        plan_missing_target_conflicts(&scope.mount, &facts, &mut plan);
        plan_lossless_repairs(&scope.mount, &facts, &mut plan);
        plan_create_collisions(state_root, &scope.mount, &facts, &mut plan)?;
    }

    Ok(plan)
}

fn plan_multi_mutation_conflicts(
    mount: &MountConfig,
    facts: &ProjectionStateFacts,
    plan: &mut ProjectionStatePlan,
) {
    for (path, mutations) in &facts.mutations_by_path {
        if mutations.len() <= 1 {
            continue;
        }
        let mut kinds = mutations
            .iter()
            .map(|mutation| mutation_kind_name(&mutation.mutation_kind))
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        kinds.dedup();
        let code = if kinds == ["create"] {
            "duplicate_pending_create_path"
        } else {
            "multiple_pending_mutations_for_path"
        };
        plan.push_conflict(ProjectionStateDiagnostic {
            code: code.to_string(),
            path: path.clone(),
            local_id: Some(
                mutations
                    .iter()
                    .map(|mutation| mutation.local_id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            remote_id: None,
            message: format!(
                "multiple pending projection mutations claim `{}`",
                path.display()
            ),
            repair: None,
        });
    }

    for (remote_id, mutations) in &facts.mutations_by_remote_id {
        if mutations.len() <= 1 {
            continue;
        }
        plan.push_conflict(ProjectionStateDiagnostic {
            code: "multiple_pending_mutations_for_entity".to_string(),
            path: mutations
                .first()
                .map(|mutation| mutation.projected_path.clone())
                .unwrap_or_default(),
            local_id: Some(
                mutations
                    .iter()
                    .map(|mutation| mutation.local_id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            remote_id: Some(remote_id.0.clone()),
            message: format!(
                "multiple pending projection mutations target remote entity `{}`",
                remote_id.0
            ),
            repair: None,
        });
    }

    let _ = mount;
}

fn plan_missing_target_conflicts(
    mount: &MountConfig,
    facts: &ProjectionStateFacts,
    plan: &mut ProjectionStatePlan,
) {
    for mutation in &facts.mutations {
        match mutation.mutation_kind {
            VirtualMutationKind::Create => {
                if plan.has_conflict_for_local_id(&mutation.local_id)
                    || facts
                        .entities_by_path
                        .contains_key(&mutation.projected_path)
                {
                    continue;
                }
                let Some(parent_id) = mutation.parent_remote_id.as_ref() else {
                    plan.push_conflict(ProjectionStateDiagnostic {
                        code: "pending_create_missing_parent".to_string(),
                        path: mutation.projected_path.clone(),
                        local_id: Some(mutation.local_id.clone()),
                        remote_id: None,
                        message: "pending create has no parent remote id".to_string(),
                        repair: None,
                    });
                    continue;
                };
                if mount.remote_root_id.as_ref() == Some(parent_id)
                    || facts.entities_by_id.contains_key(parent_id)
                {
                    continue;
                }
                plan.push_conflict(ProjectionStateDiagnostic {
                    code: "pending_create_missing_parent".to_string(),
                    path: mutation.projected_path.clone(),
                    local_id: Some(mutation.local_id.clone()),
                    remote_id: Some(parent_id.0.clone()),
                    message: format!(
                        "pending create parent `{}` is missing from local state",
                        parent_id.0
                    ),
                    repair: None,
                });
            }
            VirtualMutationKind::Move | VirtualMutationKind::Rename => {
                let Some(remote_id) = mutation.target_remote_id.as_ref() else {
                    plan.push_conflict(ProjectionStateDiagnostic {
                        code: "pending_rename_missing_target".to_string(),
                        path: mutation.projected_path.clone(),
                        local_id: Some(mutation.local_id.clone()),
                        remote_id: None,
                        message: "pending rename has no target remote id".to_string(),
                        repair: None,
                    });
                    continue;
                };
                if !facts.entities_by_id.contains_key(remote_id) {
                    plan.push_conflict(ProjectionStateDiagnostic {
                        code: "pending_rename_missing_target".to_string(),
                        path: mutation.projected_path.clone(),
                        local_id: Some(mutation.local_id.clone()),
                        remote_id: Some(remote_id.0.clone()),
                        message: format!(
                            "pending rename target `{}` is missing from local state",
                            remote_id.0
                        ),
                        repair: None,
                    });
                }
            }
            VirtualMutationKind::Delete => {}
        }
    }
}

fn plan_lossless_repairs(
    mount: &MountConfig,
    facts: &ProjectionStateFacts,
    plan: &mut ProjectionStatePlan,
) {
    for mutation in &facts.mutations {
        match mutation.mutation_kind {
            VirtualMutationKind::Delete => {
                let Some(remote_id) = mutation.target_remote_id.as_ref() else {
                    continue;
                };
                if plan.has_conflict_for_local_id(&mutation.local_id) {
                    continue;
                }
                if !facts.entities_by_id.contains_key(remote_id) {
                    plan.push_repair(
                        ProjectionStateRepair {
                            mount_id: mount.mount_id.clone(),
                            local_id: mutation.local_id.clone(),
                        },
                        ProjectionStateDiagnostic {
                            code: "orphan_pending_delete".to_string(),
                            path: mutation.projected_path.clone(),
                            local_id: Some(mutation.local_id.clone()),
                            remote_id: Some(remote_id.0.clone()),
                            message: "pending delete target is already absent from local state"
                                .to_string(),
                            repair: Some(ProjectionStateRepairKind::ClearOrphanPendingDelete),
                        },
                    );
                } else if stale_pending_delete_target_present(mount, mutation) {
                    plan.push_repair(
                        ProjectionStateRepair {
                            mount_id: mount.mount_id.clone(),
                            local_id: mutation.local_id.clone(),
                        },
                        ProjectionStateDiagnostic {
                            code: "stale_pending_delete".to_string(),
                            path: mutation.projected_path.clone(),
                            local_id: Some(mutation.local_id.clone()),
                            remote_id: Some(remote_id.0.clone()),
                            message:
                                "pending delete target is still present with matching identity"
                                    .to_string(),
                            repair: Some(ProjectionStateRepairKind::ClearStalePendingDelete),
                        },
                    );
                }
            }
            VirtualMutationKind::Move | VirtualMutationKind::Rename => {
                let Some(remote_id) = mutation.target_remote_id.as_ref() else {
                    continue;
                };
                let Some(entity) = facts.entities_by_id.get(remote_id) else {
                    continue;
                };
                if entity.path == mutation.projected_path
                    && entity.title == mutation.title
                    && !plan.has_conflict_for_local_id(&mutation.local_id)
                {
                    plan.push_repair(
                        ProjectionStateRepair {
                            mount_id: mount.mount_id.clone(),
                            local_id: mutation.local_id.clone(),
                        },
                        ProjectionStateDiagnostic {
                            code: "redundant_pending_rename".to_string(),
                            path: mutation.projected_path.clone(),
                            local_id: Some(mutation.local_id.clone()),
                            remote_id: Some(remote_id.0.clone()),
                            message: "pending rename is already reflected in local entity state"
                                .to_string(),
                            repair: Some(ProjectionStateRepairKind::ClearRedundantPendingRename),
                        },
                    );
                } else if let Some(colliding) = facts.entities_by_path.get(&mutation.projected_path)
                    && colliding.remote_id != *remote_id
                {
                    plan.push_conflict(ProjectionStateDiagnostic {
                        code: "pending_rename_path_conflict".to_string(),
                        path: mutation.projected_path.clone(),
                        local_id: Some(mutation.local_id.clone()),
                        remote_id: Some(remote_id.0.clone()),
                        message: format!(
                            "pending rename target path belongs to remote entity `{}`",
                            colliding.remote_id.0
                        ),
                        repair: None,
                    });
                }
            }
            VirtualMutationKind::Create => {}
        }
    }
}

fn plan_create_collisions(
    state_root: Option<&Path>,
    mount: &MountConfig,
    facts: &ProjectionStateFacts,
    plan: &mut ProjectionStatePlan,
) -> LocalityResult<()> {
    for mutation in &facts.mutations {
        if mutation.mutation_kind != VirtualMutationKind::Create
            || plan.has_conflict_for_local_id(&mutation.local_id)
        {
            continue;
        }
        let Some(entity) = facts.entities_by_path.get(&mutation.projected_path) else {
            continue;
        };
        let identity = pending_create_identity(mount, state_root, mutation)?;
        match identity {
            PendingCreateIdentity::Remote(remote_id) if remote_id == entity.remote_id => {
                plan.push_repair(
                    ProjectionStateRepair {
                        mount_id: mount.mount_id.clone(),
                        local_id: mutation.local_id.clone(),
                    },
                    ProjectionStateDiagnostic {
                        code: "redundant_pending_create".to_string(),
                        path: mutation.projected_path.clone(),
                        local_id: Some(mutation.local_id.clone()),
                        remote_id: Some(entity.remote_id.0.clone()),
                        message: "pending local create duplicates an existing tracked entity"
                            .to_string(),
                        repair: Some(ProjectionStateRepairKind::ClearRedundantPendingCreate),
                    },
                );
            }
            PendingCreateIdentity::Remote(remote_id) => {
                let tracked_remote_id = entity.remote_id.0.clone();
                plan.push_conflict(ProjectionStateDiagnostic {
                    code: "pending_create_identity_conflict".to_string(),
                    path: mutation.projected_path.clone(),
                    local_id: Some(mutation.local_id.clone()),
                    remote_id: Some(tracked_remote_id.clone()),
                    message: format!(
                        "pending local create carries loc.id `{}` but the path belongs to `{}`",
                        remote_id.0, tracked_remote_id
                    ),
                    repair: None,
                });
            }
            PendingCreateIdentity::ConflictingSources(remote_ids) => {
                plan.push_conflict(ProjectionStateDiagnostic {
                    code: "pending_create_identity_source_conflict".to_string(),
                    path: mutation.projected_path.clone(),
                    local_id: Some(mutation.local_id.clone()),
                    remote_id: Some(
                        remote_ids
                            .iter()
                            .map(|remote_id| remote_id.0.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    message: "pending local create content sources disagree about loc.id"
                        .to_string(),
                    repair: None,
                });
            }
            PendingCreateIdentity::MissingIdentity => {
                plan.push_conflict(ProjectionStateDiagnostic {
                    code: "pending_create_path_conflict".to_string(),
                    path: mutation.projected_path.clone(),
                    local_id: Some(mutation.local_id.clone()),
                    remote_id: Some(entity.remote_id.0.clone()),
                    message: "pending local create has no loc.id but the path already belongs to a tracked entity".to_string(),
                    repair: None,
                });
            }
            PendingCreateIdentity::Unreadable => {
                plan.push_conflict(ProjectionStateDiagnostic {
                    code: "pending_create_unreadable".to_string(),
                    path: mutation.projected_path.clone(),
                    local_id: Some(mutation.local_id.clone()),
                    remote_id: Some(entity.remote_id.0.clone()),
                    message: "pending local create collides with a tracked entity but its Markdown content could not be read".to_string(),
                    repair: None,
                });
            }
            PendingCreateIdentity::InvalidCanonical => {
                plan.push_conflict(ProjectionStateDiagnostic {
                    code: "pending_create_invalid_canonical".to_string(),
                    path: mutation.projected_path.clone(),
                    local_id: Some(mutation.local_id.clone()),
                    remote_id: Some(entity.remote_id.0.clone()),
                    message: "pending local create collides with a tracked entity but its Markdown identity could not be parsed".to_string(),
                    repair: None,
                });
            }
        }
    }
    Ok(())
}

impl ProjectionStatePlan {
    fn push_repair(
        &mut self,
        repair: ProjectionStateRepair,
        diagnostic: ProjectionStateDiagnostic,
    ) {
        if self.has_conflict_for_local_id(&repair.local_id) {
            return;
        }
        self.repairs.push(repair);
        self.diagnostics.push(diagnostic);
    }

    fn push_conflict(&mut self, diagnostic: ProjectionStateDiagnostic) {
        if self.diagnostics.iter().any(|existing| {
            existing.repair.is_none()
                && existing.code == diagnostic.code
                && existing.path == diagnostic.path
                && existing.local_id == diagnostic.local_id
                && existing.remote_id == diagnostic.remote_id
        }) {
            return;
        }
        self.diagnostics.push(diagnostic);
    }

    fn has_conflict_for_local_id(&self, local_id: &str) -> bool {
        self.diagnostics.iter().any(|diagnostic| {
            diagnostic.repair.is_none()
                && diagnostic
                    .local_id
                    .as_deref()
                    .is_some_and(|ids| ids.split(',').any(|candidate| candidate == local_id))
        })
    }
}

impl ProjectionStateFacts {
    fn load<S>(store: &S, scope: &ProjectionStateScope) -> LocalityResult<Self>
    where
        S: EntityRepository + VirtualMutationRepository,
    {
        let entities = store
            .list_entities(&scope.mount.mount_id)?
            .into_iter()
            .collect::<Vec<_>>();
        let mut entities_by_id = BTreeMap::new();
        let mut entities_by_path = BTreeMap::new();
        for entity in entities {
            entities_by_id.insert(entity.remote_id.clone(), entity.clone());
            entities_by_path.insert(entity.path.clone(), entity);
        }

        let mutations = store
            .list_virtual_mutations(&scope.mount.mount_id)?
            .into_iter()
            .filter(|mutation| scope.filter.contains(&mutation.projected_path))
            .collect::<Vec<_>>();
        let mut mutations_by_path = BTreeMap::<PathBuf, Vec<VirtualMutationRecord>>::new();
        let mut mutations_by_remote_id = BTreeMap::<RemoteId, Vec<VirtualMutationRecord>>::new();
        for mutation in &mutations {
            mutations_by_path
                .entry(mutation.projected_path.clone())
                .or_default()
                .push(mutation.clone());
            if let Some(remote_id) = mutation.target_remote_id.as_ref() {
                mutations_by_remote_id
                    .entry(remote_id.clone())
                    .or_default()
                    .push(mutation.clone());
            }
        }

        Ok(Self {
            entities_by_id,
            entities_by_path,
            mutations,
            mutations_by_path,
            mutations_by_remote_id,
        })
    }
}

fn mutation_kind_name(kind: &VirtualMutationKind) -> &'static str {
    match kind {
        VirtualMutationKind::Create => "create",
        VirtualMutationKind::Move => "move",
        VirtualMutationKind::Rename => "rename",
        VirtualMutationKind::Delete => "delete",
    }
}

fn projection_state_scopes<S>(
    store: &S,
    target: Option<&Path>,
) -> LocalityResult<Vec<ProjectionStateScope>>
where
    S: MountRepository,
{
    let mounts = store.load_mounts()?;
    let Some(target) = target.map(absolute_path).transpose()? else {
        return Ok(mounts
            .into_iter()
            .map(|mount| ProjectionStateScope {
                mount,
                filter: ProjectionStateFilter::All,
            })
            .collect());
    };

    Ok(mounts
        .into_iter()
        .filter_map(|mount| {
            file_provider::match_mount_path(&mount, &target).map(|matched| {
                let filter = if matched.relative_path.as_os_str().is_empty() {
                    ProjectionStateFilter::All
                } else if target.is_dir() {
                    ProjectionStateFilter::Subtree(matched.relative_path)
                } else {
                    ProjectionStateFilter::Exact(matched.relative_path)
                };
                ProjectionStateScope { mount, filter }
            })
        })
        .collect())
}

fn pending_create_identity(
    mount: &MountConfig,
    state_root: Option<&Path>,
    mutation: &VirtualMutationRecord,
) -> LocalityResult<PendingCreateIdentity> {
    let mut paths = Vec::new();
    if let Some(path) = mutation.content_path.as_ref() {
        paths.push(path.clone());
    }
    if let Some(state_root) = state_root {
        paths.push(virtual_fs_content_path(
            state_root,
            &mount.mount_id,
            &mutation.projected_path,
        )?);
    }
    paths.push(mount.root.join(&mutation.projected_path));
    dedupe_paths(&mut paths);

    let mut saw_readable_without_identity = false;
    let mut saw_invalid = false;
    let mut remote_ids = Vec::<RemoteId>::new();

    for path in paths {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse_canonical_markdown(&contents) {
            Ok(parsed) => {
                if let Some(remote_id) = parsed.remote_id().cloned() {
                    if !remote_ids.contains(&remote_id) {
                        remote_ids.push(remote_id);
                    }
                    continue;
                }
                saw_readable_without_identity = true;
            }
            Err(_) => saw_invalid = true,
        }
    }

    if remote_ids.len() > 1 {
        Ok(PendingCreateIdentity::ConflictingSources(remote_ids))
    } else if let Some(remote_id) = remote_ids.into_iter().next() {
        Ok(PendingCreateIdentity::Remote(remote_id))
    } else if saw_readable_without_identity {
        Ok(PendingCreateIdentity::MissingIdentity)
    } else if saw_invalid {
        Ok(PendingCreateIdentity::InvalidCanonical)
    } else {
        Ok(PendingCreateIdentity::Unreadable)
    }
}

pub(crate) fn stale_pending_delete_target_present(
    mount: &MountConfig,
    mutation: &VirtualMutationRecord,
) -> bool {
    let Some(remote_id) = mutation.target_remote_id.as_ref() else {
        return false;
    };
    let path = mount.root.join(&mutation.projected_path);
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(parsed) = parse_canonical_markdown(&contents) else {
        return false;
    };
    parsed.remote_id().is_some_and(|id| id == remote_id)
}

fn report_from_plan(plan: &ProjectionStatePlan, repaired: usize) -> ProjectionStateReconcileReport {
    ProjectionStateReconcileReport {
        checked: plan.checked,
        repaired,
        conflicts: plan
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.repair.is_none())
            .count(),
        diagnostics: plan.diagnostics.clone(),
    }
}

impl ProjectionStateFilter {
    fn contains(&self, path: &Path) -> bool {
        match self {
            ProjectionStateFilter::All => true,
            ProjectionStateFilter::Exact(exact) => path == exact,
            ProjectionStateFilter::Subtree(root) => path.starts_with(root),
        }
    }
}

fn absolute_path(path: &Path) -> LocalityResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut unique = Vec::new();
    for path in paths.drain(..) {
        if !unique.iter().any(|existing: &PathBuf| existing == &path) {
            unique.push(path);
        }
    }
    *paths = unique;
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
    use locality_store::{
        EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
        VirtualMutationKind, VirtualMutationRecord, VirtualMutationRepository,
    };

    use super::*;

    #[test]
    fn repair_clears_redundant_create_when_visible_file_identity_matches_tracked_entity() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        fixture.write_file(
            "Roadmap/page.md",
            canonical_markdown("page-1", "Roadmap", "Tracked body."),
        );
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Roadmap/page.md", None))
            .expect("save mutation");

        let report = reconcile_projection_state_for_target(
            &mut store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("reconcile state");

        assert_eq!(report.repaired, 1, "{report:#?}");
        assert_eq!(report.conflicts, 0, "{report:#?}");
        assert!(
            store
                .get_virtual_mutation(&fixture.mount_id, "local:create")
                .expect("load mutation")
                .is_none()
        );
    }

    #[test]
    fn diagnose_keeps_pending_create_when_identity_points_to_different_remote() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        fixture.write_file(
            "Roadmap/page.md",
            canonical_markdown("page-2", "Roadmap", "Different identity."),
        );
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Roadmap/page.md", None))
            .expect("save mutation");

        let report = diagnose_projection_state_for_target(
            &store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(
            report.diagnostics[0].code,
            "pending_create_identity_conflict"
        );
    }

    #[test]
    fn diagnose_keeps_pending_create_without_identity_as_path_conflict() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        fixture.write_file(
            "Roadmap/page.md",
            "---\ntitle: Roadmap\n---\nNo identity.\n",
        );
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Roadmap/page.md", None))
            .expect("save mutation");

        let report = diagnose_projection_state_for_target(
            &store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].code, "pending_create_path_conflict");
    }

    #[test]
    fn repair_clears_orphan_delete_for_missing_entity() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        store
            .save_virtual_mutation(fixture.pending_delete(
                "delete:missing-page",
                "Archived/page.md",
                "missing-page",
            ))
            .expect("save mutation");

        let report =
            reconcile_projection_state_for_target(&mut store, Some(&fixture.state_root), None)
                .expect("reconcile state");

        assert_eq!(report.repaired, 1, "{report:#?}");
        assert_eq!(report.conflicts, 0, "{report:#?}");
        assert!(
            store
                .get_virtual_mutation(&fixture.mount_id, "delete:missing-page")
                .expect("load mutation")
                .is_none()
        );
    }

    #[test]
    fn repair_clears_pending_delete_when_visible_file_identity_matches_tracked_entity() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        fixture.write_file(
            "Roadmap/page.md",
            canonical_markdown("page-1", "Roadmap", "Edited body."),
        );
        store
            .save_virtual_mutation(fixture.pending_delete(
                "delete:page-1",
                "Roadmap/page.md",
                "page-1",
            ))
            .expect("save mutation");

        let report = reconcile_projection_state_for_target(
            &mut store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("reconcile state");

        assert_eq!(report.repaired, 1, "{report:#?}");
        assert_eq!(report.conflicts, 0, "{report:#?}");
        assert!(
            store
                .get_virtual_mutation(&fixture.mount_id, "delete:page-1")
                .expect("load mutation")
                .is_none()
        );
    }

    #[test]
    fn diagnose_reports_duplicate_pending_creates_for_same_path_as_conflict() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.write_file("Draft/page.md", "---\ntitle: Draft\n---\nDraft.\n");
        store
            .save_virtual_mutation(fixture.pending_create("local:a", "Draft/page.md", None))
            .expect("save mutation a");
        store
            .save_virtual_mutation(fixture.pending_create("local:b", "Draft/page.md", None))
            .expect("save mutation b");

        let report = diagnose_projection_state_for_target(&store, Some(&fixture.state_root), None)
            .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].code, "duplicate_pending_create_path");
    }

    #[test]
    fn diagnose_reports_create_that_claims_existing_path_without_readable_content() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Roadmap/page.md", None))
            .expect("save mutation");

        let report = diagnose_projection_state_for_target(
            &store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].code, "pending_create_unreadable");
    }

    #[test]
    fn diagnose_reports_create_that_claims_existing_path_with_invalid_canonical_content() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        fixture.write_file("Roadmap/page.md", "missing frontmatter\n");
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Roadmap/page.md", None))
            .expect("save mutation");

        let report = diagnose_projection_state_for_target(
            &store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(
            report.diagnostics[0].code,
            "pending_create_invalid_canonical"
        );
    }

    #[test]
    fn diagnose_reports_rename_whose_target_entity_is_missing() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        store
            .save_virtual_mutation(fixture.pending_rename(
                "rename:missing-page",
                "Renamed/page.md",
                "missing-page",
            ))
            .expect("save rename mutation");

        let report = diagnose_projection_state_for_target(&store, Some(&fixture.state_root), None)
            .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].code, "pending_rename_missing_target");
    }

    #[test]
    fn diagnose_reports_rename_projected_path_collision_with_another_entity() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        fixture.entity(&mut store, "page-2", "Existing/page.md");
        store
            .save_virtual_mutation(fixture.pending_rename(
                "rename:page-1",
                "Existing/page.md",
                "page-1",
            ))
            .expect("save rename mutation");

        let report = diagnose_projection_state_for_target(&store, Some(&fixture.state_root), None)
            .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].code, "pending_rename_path_conflict");
    }

    #[test]
    fn repair_clears_redundant_rename_when_entity_already_has_projected_path_and_title() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity_with_title(&mut store, "page-1", "Renamed", "Renamed/page.md");
        store
            .save_virtual_mutation(fixture.pending_rename(
                "rename:page-1",
                "Renamed/page.md",
                "page-1",
            ))
            .expect("save rename mutation");

        let report =
            reconcile_projection_state_for_target(&mut store, Some(&fixture.state_root), None)
                .expect("reconcile state");

        assert_eq!(report.repaired, 1, "{report:#?}");
        assert_eq!(report.conflicts, 0, "{report:#?}");
        assert!(
            store
                .get_virtual_mutation(&fixture.mount_id, "rename:page-1")
                .expect("load mutation")
                .is_none()
        );
    }

    #[test]
    fn diagnose_reports_delete_and_rename_for_same_entity_as_conflict() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        store
            .save_virtual_mutation(fixture.pending_delete(
                "delete:page-1",
                "Roadmap/page.md",
                "page-1",
            ))
            .expect("save delete mutation");
        store
            .save_virtual_mutation(fixture.pending_rename(
                "rename:page-1",
                "Renamed/page.md",
                "page-1",
            ))
            .expect("save rename mutation");

        let report = diagnose_projection_state_for_target(&store, Some(&fixture.state_root), None)
            .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(
            report.diagnostics[0].code,
            "multiple_pending_mutations_for_entity"
        );
    }

    #[test]
    fn diagnose_reports_create_and_delete_for_same_projected_path_as_conflict() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        store
            .save_virtual_mutation(fixture.pending_delete(
                "delete:page-1",
                "Roadmap/page.md",
                "page-1",
            ))
            .expect("save delete mutation");
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Roadmap/page.md", None))
            .expect("save create mutation");

        let report = diagnose_projection_state_for_target(&store, Some(&fixture.state_root), None)
            .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(
            report.diagnostics[0].code,
            "multiple_pending_mutations_for_path"
        );
    }

    #[test]
    fn diagnose_reports_create_with_missing_parent_remote() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.write_file("Draft/page.md", "---\ntitle: Draft\n---\nDraft.\n");
        store
            .save_virtual_mutation(fixture.pending_create("local:create", "Draft/page.md", None))
            .expect("save mutation");

        let report = diagnose_projection_state_for_target(&store, Some(&fixture.state_root), None)
            .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(report.diagnostics[0].code, "pending_create_missing_parent");
    }

    #[test]
    fn diagnose_reports_create_content_path_identity_disagreeing_with_visible_file_identity() {
        let fixture = Fixture::new();
        let mut store = fixture.store();
        fixture.entity(&mut store, "page-1", "Roadmap/page.md");
        let content_path = fixture.write_state_file(
            "Roadmap/page.md",
            canonical_markdown("page-2", "Roadmap", "Cache body."),
        );
        fixture.write_file(
            "Roadmap/page.md",
            canonical_markdown("page-1", "Roadmap", "Visible body."),
        );
        store
            .save_virtual_mutation(fixture.pending_create(
                "local:create",
                "Roadmap/page.md",
                Some(content_path),
            ))
            .expect("save mutation");

        let report = diagnose_projection_state_for_target(
            &store,
            Some(&fixture.state_root),
            Some(&fixture.root.join("Roadmap/page.md")),
        )
        .expect("diagnose state");

        assert_eq!(report.repaired, 0, "{report:#?}");
        assert_eq!(report.conflicts, 1, "{report:#?}");
        assert_eq!(
            report.diagnostics[0].code,
            "pending_create_identity_source_conflict"
        );
    }

    struct Fixture {
        root: PathBuf,
        state_root: PathBuf,
        mount_id: MountId,
    }

    impl Fixture {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "loc-projection-state-{}-{unique}-{suffix}",
                std::process::id()
            ));
            let state_root = std::env::temp_dir().join(format!(
                "loc-projection-state-root-{}-{unique}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("root");
            fs::create_dir_all(&state_root).expect("state root");
            Self {
                root,
                state_root,
                mount_id: MountId::new("notion-main"),
            }
        }

        fn store(&self) -> InMemoryStateStore {
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(MountConfig::new(
                    self.mount_id.clone(),
                    "notion",
                    self.root.clone(),
                ))
                .expect("save mount");
            store
        }

        fn entity(&self, store: &mut InMemoryStateStore, remote_id: &str, path: &str) {
            self.entity_with_title(store, remote_id, "Roadmap", path);
        }

        fn entity_with_title(
            &self,
            store: &mut InMemoryStateStore,
            remote_id: &str,
            title: &str,
            path: &str,
        ) {
            store
                .save_entity(
                    EntityRecord::new(
                        self.mount_id.clone(),
                        RemoteId::new(remote_id),
                        EntityKind::Page,
                        title,
                        path,
                    )
                    .with_hydration(HydrationState::Hydrated),
                )
                .expect("save entity");
        }

        fn write_file(&self, relative_path: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.root.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(&path, contents).expect("write file");
            path
        }

        fn write_state_file(&self, relative_path: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path =
                virtual_fs_content_path(&self.state_root, &self.mount_id, Path::new(relative_path))
                    .expect("state content path");
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("state parent");
            }
            fs::write(&path, contents).expect("write state file");
            path
        }

        fn pending_create(
            &self,
            local_id: &str,
            path: &str,
            content_path: Option<PathBuf>,
        ) -> VirtualMutationRecord {
            VirtualMutationRecord {
                mount_id: self.mount_id.clone(),
                local_id: local_id.to_string(),
                mutation_kind: VirtualMutationKind::Create,
                target_remote_id: None,
                parent_remote_id: Some(RemoteId::new("parent")),
                original_path: None,
                projected_path: PathBuf::from(path),
                title: "Draft".to_string(),
                content_path,
                created_at: "now".to_string(),
                updated_at: "now".to_string(),
            }
        }

        fn pending_delete(
            &self,
            local_id: &str,
            path: &str,
            target_remote_id: &str,
        ) -> VirtualMutationRecord {
            VirtualMutationRecord {
                mount_id: self.mount_id.clone(),
                local_id: local_id.to_string(),
                mutation_kind: VirtualMutationKind::Delete,
                target_remote_id: Some(RemoteId::new(target_remote_id)),
                parent_remote_id: None,
                original_path: None,
                projected_path: PathBuf::from(path),
                title: "Archived".to_string(),
                content_path: None,
                created_at: "now".to_string(),
                updated_at: "now".to_string(),
            }
        }

        fn pending_rename(
            &self,
            local_id: &str,
            path: &str,
            target_remote_id: &str,
        ) -> VirtualMutationRecord {
            VirtualMutationRecord {
                mount_id: self.mount_id.clone(),
                local_id: local_id.to_string(),
                mutation_kind: VirtualMutationKind::Rename,
                target_remote_id: Some(RemoteId::new(target_remote_id)),
                parent_remote_id: None,
                original_path: Some(PathBuf::from("original/page.md")),
                projected_path: PathBuf::from(path),
                title: "Renamed".to_string(),
                content_path: None,
                created_at: "now".to_string(),
                updated_at: "now".to_string(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.state_root);
        }
    }

    fn canonical_markdown(remote_id: &str, title: &str, body: &str) -> String {
        format!(
            "---\nloc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: {title}\n---\n{body}\n"
        )
    }
}
