//! The address ledger, `kiln.lock`.
//!
//! Flattening functions into resources means a refactoring can change a
//! resource's address, and Terraform reads a changed address as "destroy the
//! old one, create a new one". The ledger records every address a stack has
//! emitted. Each build diffs against it: a declared rename becomes a `moved`
//! block, and an undeclared one stops the build.

use crate::eval::Graph;
use serde_json::{json, Value as J};
use std::collections::BTreeSet;

pub type Move = (String, String);

#[derive(Default, Debug, Clone, PartialEq)]
pub struct Ledger {
    pub addresses: BTreeSet<String>,
    /// `moved` blocks to keep emitting until every state has applied them.
    pub moved: Vec<Move>,
    /// Renames declared with `kiln mv`, consumed by the next build.
    pub pending: Vec<Move>,
}

fn moves_json(ms: &[Move]) -> J {
    J::Array(ms.iter().map(|(f, t)| json!({ "from": f, "to": t })).collect())
}

fn moves_from(j: Option<&J>) -> Result<Vec<Move>, String> {
    let mut out = vec![];
    for m in j.and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
        match (m["from"].as_str(), m["to"].as_str()) {
            (Some(f), Some(t)) => out.push((f.to_string(), t.to_string())),
            _ => return Err("kiln.lock: a move needs \"from\" and \"to\"".into()),
        }
    }
    Ok(out)
}

pub fn addr_type(a: &str) -> &str {
    if a.starts_with("module.") {
        "module"
    } else {
        a.split('.').next().unwrap_or(a)
    }
}

impl Ledger {
    pub fn parse(text: &str) -> Result<Ledger, String> {
        let j: J = serde_json::from_str(text).map_err(|e| format!("kiln.lock is not valid JSON: {}", e))?;
        if j["version"] != json!(1) {
            return Err("kiln.lock: unsupported version (this kiln reads version 1)".into());
        }
        let addresses = j["addresses"]
            .as_array()
            .ok_or("kiln.lock: missing \"addresses\"")?
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect();
        Ok(Ledger {
            addresses,
            moved: moves_from(j.get("moved"))?,
            pending: moves_from(j.get("pending"))?,
        })
    }

    pub fn render(&self) -> String {
        let mut j = json!({
            "version": 1,
            "addresses": self.addresses.iter().collect::<Vec<_>>(),
            "moved": moves_json(&self.moved),
        });
        if !self.pending.is_empty() {
            j["pending"] = moves_json(&self.pending);
        }
        let mut s = serde_json::to_string_pretty(&j).expect("JSON serialisation");
        s.push('\n');
        s
    }
}

/// How alike two declarations are, 0.0 to 1.0: the share of top-level
/// arguments with equal values, or failing that the shared tail of the names.
fn similarity(old_addr: &str, new_addr: &str, old_body: Option<&J>, new_body: Option<&J>) -> f64 {
    if let (Some(J::Object(a)), Some(J::Object(b))) = (old_body, new_body) {
        let total = a.len().max(b.len());
        if total > 0 {
            let same = a.iter().filter(|(k, v)| b.get(*k) == Some(*v)).count();
            return same as f64 / total as f64;
        }
    }
    let (a, b) = (old_addr.as_bytes(), new_addr.as_bytes());
    let tail = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    tail as f64 / a.len().max(b.len()).max(1) as f64
}

fn old_body<'a>(old_output: Option<&'a J>, address: &str) -> Option<&'a J> {
    let root = old_output?;
    if let Some(name) = address.strip_prefix("module.") {
        return root.get("module")?.get(name);
    }
    let (ty, name) = address.split_once('.')?;
    root.get("resource")?.get(ty)?.get(name)
}

/// Diff the graph against the previous ledger. Returns the new ledger and the
/// moves this build introduced.
pub fn reconcile(
    old: Option<&Ledger>,
    g: &Graph,
    old_output: Option<&str>,
    accept_replace: bool,
) -> Result<(Ledger, Vec<Move>), String> {
    let new = g.addresses();
    let old = match old {
        Some(o) => o,
        None => return Ok((Ledger { addresses: new, ..Ledger::default() }, vec![])),
    };
    let mut removed: BTreeSet<String> = old.addresses.difference(&new).cloned().collect();
    let mut added: BTreeSet<String> = new.difference(&old.addresses).cloned().collect();
    let mut moves: Vec<Move> = vec![];

    // 1. Renames declared in the source with ^{:was "..."}.
    for (addr, olds) in &g.renames {
        if !added.contains(addr) {
            continue;
        }
        if let Some(w) = olds.iter().find(|w| removed.contains(*w)) {
            removed.remove(w);
            added.remove(addr);
            moves.push((w.clone(), addr.clone()));
        }
    }

    // 2. Renames declared with `kiln mv`.
    let mut stale = vec![];
    for (f, t) in &old.pending {
        if addr_type(f) != addr_type(t) {
            return Err(format!(
                "error[K0502]: kiln mv {} {} changes the resource type; Terraform cannot move state across types\n",
                f, t
            ));
        }
        if removed.contains(f) && added.contains(t) {
            removed.remove(f);
            added.remove(t);
            moves.push((f.clone(), t.clone()));
        } else {
            stale.push(format!("{} -> {}", f, t));
        }
    }
    if !stale.is_empty() {
        return Err(format!(
            "error[K0501]: a move declared with `kiln mv` does not match this build\n{}   = help: \
             a move applies when its source address disappears and its target appears in the same build;\n           \
             delete the entry from \"pending\" in kiln.lock if it is no longer wanted\n",
            stale.iter().map(|s| format!("   = {}\n", s)).collect::<String>()
        ));
    }

    // 3. Anything left that looks like a rename stops the build.
    if !accept_replace {
        let old_json: Option<J> = old_output.and_then(|t| serde_json::from_str(t).ok());
        let mut proposals = vec![];
        let mut taken: BTreeSet<&String> = BTreeSet::new();
        for r in &removed {
            let best = added
                .iter()
                .filter(|a| addr_type(a) == addr_type(r) && !taken.contains(a))
                .map(|a| (similarity(r, a, old_body(old_json.as_ref(), r), g.body(a)), a))
                .max_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((score, a)) = best {
                taken.insert(a);
                proposals.push((r.clone(), a.clone(), score));
            }
        }
        if !proposals.is_empty() {
            let mut msg = format!(
                "error[K0500]: {} address{} disappeared while new addresses of the same type appeared\n",
                proposals.len(),
                if proposals.len() == 1 { "" } else { "es" }
            );
            for (r, a, score) in &proposals {
                msg.push_str(&format!("   = {}  ->  {}   ({:.0}% alike)\n", r, a, score * 100.0));
            }
            msg.push_str(
                "   = help: Terraform would destroy the old resources and create new ones. If these are renames,\n           \
                 declare them so state is moved instead:\n             \
                 in the source:  ^{:was \"old-name\"} \"new-name\"\n             \
                 or:             kiln mv <file.kiln> <from> <to>\n           \
                 If they really are deletions and creations, rebuild with --replace.\n",
            );
            return Err(msg);
        }
    }

    let mut moved = old.moved.clone();
    for (f, t) in &moves {
        // Renaming back: the inverse move would form a cycle, so it goes.
        moved.retain(|(a, b)| !(a == t && b == f));
        if !moved.contains(&(f.clone(), t.clone())) {
            moved.push((f.clone(), t.clone()));
        }
    }
    moved.sort();
    Ok((Ledger { addresses: new, moved, pending: vec![] }, moves))
}
