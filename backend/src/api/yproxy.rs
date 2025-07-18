use crate::api::model::{Graph, Task};
use anyhow::{Context, Result, anyhow};
use similar::{Algorithm, capture_diff_slices};
use std::collections::{HashMap, HashSet};
use yrs::{
    Any, Array, ArrayRef, DeepObservable, Doc, GetString, Map, MapRef, Observable, Origin, Out,
    ReadTxn, Subscription, Text, TextRef, Transact, TransactionAcqError, TransactionMut,
    UpdateEvent,
    types::{Events, map::MapEvent},
};

// Keep this in sync with the corresponding list in
// frontend/yproxy.ts
const MANAGED_KINDS: &[&str] = &["github", "github_pr"];

pub(crate) struct YDocProxy {
    doc: Doc,
    graph: MapRef,
}

impl YDocProxy {
    pub fn new() -> Self {
        let doc = Doc::new();
        let graph = doc.get_or_insert_map("graph");
        YDocProxy { doc, graph }
    }

    pub fn new_from_existing_doc<T: ReadTxn>(doc: Doc, txn: &T) -> Result<Self> {
        Ok(YDocProxy {
            doc,
            graph: txn.get_map("graph").context("graph map missing")?,
        })
    }

    pub fn to_graph<T: ReadTxn>(&self, txn: &T) -> Result<Graph> {
        let mut graph: Graph = HashMap::new();
        for id in self.graph.keys(txn) {
            graph.insert(id.to_string(), self.get(txn, id)?.to_task(txn)?);
        }
        Ok(graph)
    }

    pub fn tasks<T: ReadTxn>(&self, txn: &T) -> Result<Vec<YTaskProxy>> {
        self.graph.keys(txn).map(|id| self.get(txn, id)).collect()
    }

    pub fn set(&self, txn: &mut yrs::TransactionMut, task: &Task) -> YTaskProxy {
        let y_task: MapRef = self.graph.get_or_init(txn, task.id.as_ref());
        let y_task = YTaskProxy::new(y_task);
        y_task.set_id(txn, &task.id);
        y_task.set_num(txn, &task.num);
        y_task.set_name(txn, &task.name);
        y_task.set_desc(txn, task.desc.as_deref());
        y_task.set_children(txn, &task.children);
        y_task.set_assignee(txn, task.assignee.as_deref());
        y_task.set_reporter(txn, task.reporter.as_deref());
        y_task.set_status(txn, task.status.as_deref());
        y_task.set_status_time(txn, task.status_time);
        y_task.set_url(txn, task.url.as_deref());
        y_task.set_kind(txn, task.kind.as_deref());
        y_task.set_estimate(txn, task.estimate);
        y_task.set_deadline(txn, task.deadline);
        y_task.set_archived(txn, task.archived);
        y_task
    }

    pub fn get<T: ReadTxn>(&self, txn: &T, id: &str) -> Result<YTaskProxy> {
        let Some(y_task) = self.graph.get(txn, id) else {
            return Err(anyhow!("task is missing: {id}"));
        };
        let y_task = match y_task {
            Out::YMap(map_ref) => map_ref,
            _ => return Err(anyhow!("task {id} is not a map")),
        };
        Ok(YTaskProxy::new(y_task))
    }

    pub fn get_by_nums<T: ReadTxn>(
        &self,
        txn: &T,
        nums: &HashSet<String>,
    ) -> Result<Vec<YTaskProxy>> {
        if nums.is_empty() {
            return Ok(Vec::with_capacity(0));
        }

        let mut tasks = Vec::with_capacity(nums.len());
        for id in self.graph.keys(txn) {
            let task = self.get(txn, id)?;
            if nums.contains(&task.get_num(txn)?) {
                tasks.push(task);
                if tasks.len() == nums.len() {
                    break;
                }
            }
        }
        Ok(tasks)
    }

    pub fn observe_update_v2<F>(&self, f: F) -> Result<Subscription, TransactionAcqError>
    where
        F: Fn(&TransactionMut, &UpdateEvent) + Send + Sync + 'static,
    {
        self.doc.observe_update_v2(f)
    }

    pub fn observe_graph<F>(&self, f: F) -> Subscription
    where
        F: Fn(&TransactionMut, &MapEvent) + Send + Sync + 'static,
    {
        self.graph.observe(f)
    }

    pub fn observe_deep_graph<F>(&self, f: F) -> Subscription
    where
        F: Fn(&TransactionMut, &Events) + Send + Sync + 'static,
    {
        self.graph.observe_deep(f)
    }

    pub fn transact(&self) -> yrs::Transaction<'_> {
        self.doc.transact()
    }

    pub fn transact_mut_with<T: Into<Origin>>(&self, origin: T) -> TransactionMut {
        self.doc.transact_mut_with(origin)
    }

    /// Returns the next available task number. i.e max(num)+1
    pub fn next_num<T: ReadTxn>(&self, txn: &T) -> Result<u64> {
        let mut max_num = 0;
        for id in self.graph.keys(txn) {
            let num = self.get(txn, id)?.get_num(txn)?.parse::<u64>()?;
            if num > max_num {
                max_num = num;
            }
        }
        Ok(max_num + 1)
    }
}

pub(crate) struct YTaskProxy {
    y_task: MapRef,
}

impl YTaskProxy {
    pub fn new(y_task: MapRef) -> Self {
        YTaskProxy { y_task }
    }

    pub fn to_task<T: ReadTxn>(&self, txn: &T) -> Result<Task> {
        Ok(Task {
            id: self.get_id(txn)?,
            num: self.get_num(txn)?,
            name: self.get_name(txn)?,
            desc: self.get_desc(txn)?,
            children: self.get_children(txn)?,
            assignee: self.get_assignee(txn)?,
            reporter: self.get_reporter(txn)?,
            status: self.get_status(txn)?,
            status_time: self.get_status_time(txn)?,
            url: self.get_url(txn)?,
            kind: self.get_kind(txn)?,
            estimate: self.get_estimate(txn)?,
            deadline: self.get_deadline(txn)?,
            archived: self.get_archived(txn)?,
        })
    }

    fn get_optional_number<T: ReadTxn>(&self, txn: &T, field: &str) -> Result<Option<i64>> {
        let Some(result) = self.y_task.get(txn, field) else {
            return Ok(None);
        };
        match result {
            Out::Any(Any::Number(result)) => Ok(Some(result as i64)),
            Out::Any(Any::Null) | Out::Any(Any::Undefined) => Ok(None),
            _ => Err(anyhow!("invalid field: {field}: {result:?}")),
        }
    }

    fn get_optional_string<T: ReadTxn>(&self, txn: &T, field: &str) -> Result<Option<String>> {
        let Some(result) = self.y_task.get(txn, field) else {
            return Ok(None);
        };
        match result {
            Out::Any(Any::String(result)) => Ok(Some(result.to_string())),
            Out::Any(Any::Null) | Out::Any(Any::Undefined) => Ok(None),
            _ => Err(anyhow!("invalid field: {field}: {result:?}")),
        }
    }

    fn get_string<T: ReadTxn>(&self, txn: &T, field: &str) -> Result<String> {
        let Some(result) = self.get_optional_string(txn, field)? else {
            return Err(anyhow!("field is missing: {field}"));
        };
        Ok(result)
    }

    fn get_optional_bool<T: ReadTxn>(&self, txn: &T, field: &str) -> Result<Option<bool>> {
        let Some(result) = self.y_task.get(txn, field) else {
            return Ok(None);
        };
        match result {
            Out::Any(Any::Bool(result)) => Ok(Some(result)),
            Out::Any(Any::Null) | Out::Any(Any::Undefined) => Ok(None),
            _ => Err(anyhow!("invalid field: {field}: {result:?}")),
        }
    }

    pub fn get_id<T: ReadTxn>(&self, txn: &T) -> Result<String> {
        self.get_string(txn, "id")
    }

    fn set_id(&self, txn: &mut TransactionMut, id: &str) {
        self.y_task.try_update(txn, "id", id);
    }

    pub fn get_num<T: ReadTxn>(&self, txn: &T) -> Result<String> {
        self.get_string(txn, "num")
    }

    pub fn set_num(&self, txn: &mut TransactionMut, num: &str) {
        self.y_task.try_update(txn, "num", num);
    }

    pub fn get_name<T: ReadTxn>(&self, txn: &T) -> Result<String> {
        self.get_string(txn, "name")
    }

    pub fn set_name(&self, txn: &mut TransactionMut, name: &str) {
        self.y_task.try_update(txn, "name", name);
    }

    pub fn get_desc<T: ReadTxn>(&self, txn: &T) -> Result<Option<String>> {
        let Some(result) = self.y_task.get(txn, "desc") else {
            return Ok(None);
        };

        match result {
            Out::YText(text_ref) => Ok(Some(text_ref.get_string(txn))),
            Out::Any(Any::Null) | Out::Any(Any::Undefined) => Ok(None),
            _ => Err(anyhow!("invalid type for desc field: {result:?}")),
        }
    }

    /// If this is ever actually used, implement diff ops.
    pub fn set_desc(&self, txn: &mut TransactionMut, desc: Option<&str>) {
        match desc {
            Some(desc) => {
                let y_desc: TextRef = self.y_task.get_or_init(txn, "desc");
                y_desc.remove_range(txn, 0, y_desc.len(txn));
                y_desc.insert(txn, 0, desc);
            }
            None => {
                self.y_task.try_update(txn, "desc", Any::Null);
            }
        };
    }

    pub fn get_children<T: ReadTxn>(&self, txn: &T) -> Result<Vec<String>> {
        let Some(y_children) = self.y_task.get(txn, "children") else {
            return Ok(Vec::new());
        };
        let Out::YArray(y_children) = y_children else {
            return Err(anyhow!("invalid field: children: {y_children}"));
        };
        y_children
            .iter(txn)
            .map(|item| match item {
                Out::Any(Any::String(s)) => Ok(s.to_string()),
                e => Err(anyhow!("invalid child: {e}")),
            })
            .collect()
    }

    pub fn set_children(&self, txn: &mut TransactionMut, new_children: &[String]) {
        let y_children: ArrayRef = self.y_task.get_or_init(txn, "children");

        let old_children = match self.get_children(txn) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("invalid children, clobbering children: {e:?}");
                y_children.remove_range(txn, 0, y_children.len(txn));
                y_children.insert_range(txn, 0, new_children.to_vec());
                return;
            }
        };

        if old_children != *new_children {
            let ops = capture_diff_slices(Algorithm::Myers, &old_children, new_children)
                .into_iter()
                .rev()
                .collect::<Vec<_>>();

            for ops in ops {
                match ops {
                    similar::DiffOp::Delete {
                        old_index, old_len, ..
                    } => {
                        y_children.remove_range(txn, old_index as u32, old_len as u32);
                    }
                    similar::DiffOp::Insert {
                        old_index,
                        new_index,
                        new_len,
                    } => {
                        y_children.insert_range(
                            txn,
                            old_index as u32,
                            new_children[new_index..(new_index + new_len)].to_vec(),
                        );
                    }
                    similar::DiffOp::Replace {
                        old_index,
                        old_len,
                        new_index,
                        new_len,
                    } => {
                        y_children.remove_range(txn, old_index as u32, old_len as u32);
                        y_children.insert_range(
                            txn,
                            old_index as u32,
                            new_children[new_index..(new_index + new_len)].to_vec(),
                        );
                    }
                    _ => (),
                }
            }
        }
    }

    /// Appends the given child.
    ///
    /// If the child already exists, this method returns false and leaves children unchanged.
    pub fn push_child(&self, txn: &mut TransactionMut, child: &str) -> Result<bool> {
        let y_children: ArrayRef = self.y_task.get_or_init(txn, "children");

        if y_children.iter(txn).any(|item| match item {
            Out::Any(Any::String(item)) => item.as_ref() == child,
            _ => false,
        }) {
            return Ok(false);
        }
        y_children.push_back(txn, child);
        Ok(true)
    }

    pub fn get_assignee<T: ReadTxn>(&self, txn: &T) -> Result<Option<String>> {
        self.get_optional_string(txn, "assignee")
    }

    pub fn set_assignee(&self, txn: &mut TransactionMut, assignee: Option<&str>) {
        self.y_task.try_update(txn, "assignee", assignee);
    }

    pub fn get_reporter<T: ReadTxn>(&self, txn: &T) -> Result<Option<String>> {
        self.get_optional_string(txn, "reporter")
    }

    pub fn set_reporter(&self, txn: &mut TransactionMut, reporter: Option<&str>) {
        self.y_task.try_update(txn, "reporter", reporter);
    }

    pub fn get_status<T: ReadTxn>(&self, txn: &T) -> Result<Option<String>> {
        self.get_optional_string(txn, "status")
    }

    pub fn set_status(&self, txn: &mut TransactionMut, status: Option<&str>) {
        self.y_task.try_update(txn, "status", status);
    }

    pub fn get_status_time<T: ReadTxn>(&self, txn: &T) -> Result<Option<i64>> {
        self.get_optional_number(txn, "statusTime")
    }

    pub fn set_status_time(&self, txn: &mut TransactionMut, status_time: Option<i64>) {
        self.y_task.try_update(txn, "statusTime", status_time);
    }

    pub fn get_url<T: ReadTxn>(&self, txn: &T) -> Result<Option<String>> {
        self.get_optional_string(txn, "url")
    }

    pub fn set_url(&self, txn: &mut TransactionMut, url: Option<&str>) {
        self.y_task.try_update(txn, "url", url);
    }

    pub fn get_kind<T: ReadTxn>(&self, txn: &T) -> Result<Option<String>> {
        self.get_optional_string(txn, "kind")
    }

    pub fn set_kind(&self, txn: &mut TransactionMut, kind: Option<&str>) {
        self.y_task.try_update(txn, "kind", kind);
    }

    pub fn get_estimate<T: ReadTxn>(&self, txn: &T) -> Result<Option<i64>> {
        self.get_optional_number(txn, "estimate")
    }

    pub fn set_estimate(&self, txn: &mut TransactionMut, status_time: Option<i64>) {
        self.y_task.try_update(txn, "estimate", status_time);
    }

    pub fn get_deadline<T: ReadTxn>(&self, txn: &T) -> Result<Option<i64>> {
        self.get_optional_number(txn, "deadline")
    }

    pub fn set_deadline(&self, txn: &mut TransactionMut, status_time: Option<i64>) {
        self.y_task.try_update(txn, "deadline", status_time);
    }
    pub fn get_archived<T: ReadTxn>(&self, txn: &T) -> Result<Option<bool>> {
        self.get_optional_bool(txn, "archived")
    }

    pub fn set_archived(&self, txn: &mut TransactionMut, status_time: Option<bool>) {
        self.y_task.try_update(txn, "archived", status_time);
    }

    pub fn is_rollup<T: ReadTxn>(&self, txn: &T) -> Result<bool> {
        Ok(match self.get_kind(txn)? {
            Some(kind) => kind == "Rollup",
            None => !self.get_children(txn)?.is_empty(),
        })
    }

    pub fn is_managed<T: ReadTxn>(&self, txn: &T) -> Result<bool> {
        Ok(self
            .get_kind(txn)?
            .map(|kind| MANAGED_KINDS.contains(&kind.as_str()))
            .unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use crate::api::{
        collab::txn_origin::{self, YOrigin},
        model::test_utils::new_with_fields_populated,
    };

    use super::*;

    #[test]
    fn set_and_get_task_succeeds() {
        let ydoc = YDocProxy::new();

        let task = new_with_fields_populated();
        {
            let mut txn = ydoc.transact_mut_with(origin());
            ydoc.set(&mut txn, &task);

            assert_eq!(ydoc.get(&txn, "id1").unwrap().to_task(&txn).unwrap(), task)
        }

        // Mutate a few fields.
        let task = Task {
            name: "Task 1-edited".to_string(),
            children: vec!["2".to_string(), "3".to_string()],
            estimate: Some(8),
            archived: Some(true),
            ..task
        };
        {
            let mut txn = ydoc.transact_mut_with(origin());
            ydoc.set(&mut txn, &task);
        }

        let txn = ydoc.transact();
        let y_task = ydoc.get(&txn, "id1").unwrap();
        assert_eq!(y_task.to_task(&txn).unwrap(), task)
    }

    #[test]
    fn set_and_get_only_required_fields_succeeds() {
        let ydoc = YDocProxy::new();

        let task = Task {
            id: "id1".to_string(),
            num: "1".to_string(),
            name: "Task 1".to_string(),
            ..Task::default()
        };
        {
            let mut txn = ydoc.transact_mut_with(origin());
            ydoc.set(&mut txn, &task);

            assert_eq!(ydoc.get(&txn, "id1").unwrap().to_task(&txn).unwrap(), task)
        }
    }

    fn origin() -> Origin {
        YOrigin {
            who: "set_and_get_task_succeeds".to_string(),
            id: "test".to_string(),
            actor: txn_origin::Actor::Server,
        }
        .as_origin()
        .unwrap()
    }
}
