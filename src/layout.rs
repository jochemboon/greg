//! Editor groups (split panes) and the tree that arranges them, like VSCode's
//! editor grid. Each group has its own tabs. A tab is a *view* onto a
//! document, so one document can be shown in several groups at once, each
//! view with its own cursor and scroll position.

use eframe::egui::{Pos2, Rect};

pub type GroupId = u64;
pub type ViewId = u64;

/// Where to put a new group relative to an existing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

impl Side {
    fn axis(self) -> Axis {
        match self {
            Side::Left | Side::Right => Axis::Row,
            Side::Up | Side::Down => Axis::Column,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Children side by side (a vertical divider between them).
    Row,
    /// Children stacked (a horizontal divider between them).
    Column,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tab {
    pub view: ViewId,
    /// `Document::uid` of the document shown.
    pub doc: u64,
}

#[derive(Debug)]
pub struct Group {
    pub id: GroupId,
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl Group {
    pub fn active_tab(&self) -> Option<Tab> {
        self.tabs.get(self.active).copied()
    }

    fn position(&self, view: ViewId) -> Option<usize> {
        self.tabs.iter().position(|t| t.view == view)
    }

    /// Insert after the active tab and make it active.
    fn insert(&mut self, tab: Tab) {
        let at = if self.tabs.is_empty() {
            0
        } else {
            self.active + 1
        };
        self.tabs.insert(at, tab);
        self.active = at;
    }

    fn remove(&mut self, idx: usize) -> Tab {
        let tab = self.tabs.remove(idx);
        if idx < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        tab
    }
}

#[derive(Debug)]
pub enum Node {
    Leaf(GroupId),
    Split {
        axis: Axis,
        /// Share of the space taken by `first`.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// A draggable divider between the two halves of a split.
pub struct Splitter {
    /// Child indices (0 = first, 1 = second) leading to the split node.
    pub path: Vec<u8>,
    pub axis: Axis,
    /// The whole area of the split node.
    pub area: Rect,
    /// The gap between the two halves.
    pub line: Rect,
}

impl Node {
    fn first_leaf(&self) -> GroupId {
        match self {
            Node::Leaf(g) => *g,
            Node::Split { first, .. } => first.first_leaf(),
        }
    }

    fn order(&self, out: &mut Vec<GroupId>) {
        match self {
            Node::Leaf(g) => out.push(*g),
            Node::Split { first, second, .. } => {
                first.order(out);
                second.order(out);
            }
        }
    }

    /// Replace the leaf `target` with a split holding it and `new`.
    fn split(&mut self, target: GroupId, new: GroupId, side: Side) -> bool {
        match self {
            Node::Leaf(g) if *g == target => {
                let (a, b) = match side {
                    Side::Left | Side::Up => (new, target),
                    Side::Right | Side::Down => (target, new),
                };
                *self = Node::Split {
                    axis: side.axis(),
                    ratio: 0.5,
                    first: Box::new(Node::Leaf(a)),
                    second: Box::new(Node::Leaf(b)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                first.split(target, new, side) || second.split(target, new, side)
            }
        }
    }

    /// Remove the leaf `target`; its sibling takes the parent split's place.
    /// Returns the first group of that sibling (the one that grows into the
    /// freed space), or `None` if `target` isn't a removable leaf.
    fn remove(&mut self, target: GroupId) -> Option<GroupId> {
        let Node::Split { first, second, .. } = self else {
            return None;
        };
        let keep = if matches!(**first, Node::Leaf(g) if g == target) {
            std::mem::replace(&mut **second, Node::Leaf(0))
        } else if matches!(**second, Node::Leaf(g) if g == target) {
            std::mem::replace(&mut **first, Node::Leaf(0))
        } else {
            return first.remove(target).or_else(|| second.remove(target));
        };
        let grows = keep.first_leaf();
        *self = keep;
        Some(grows)
    }

    pub fn at_path_mut(&mut self, path: &[u8]) -> Option<&mut Node> {
        match path.split_first() {
            None => Some(self),
            Some((&i, rest)) => match self {
                Node::Split { first, second, .. } => {
                    if i == 0 { first } else { second }.at_path_mut(rest)
                }
                Node::Leaf(_) => None,
            },
        }
    }

    /// Divide `rect` among the groups, leaving `gap` between neighbours.
    pub fn layout(
        &self,
        rect: Rect,
        gap: f32,
        path: &mut Vec<u8>,
        leaves: &mut Vec<(GroupId, Rect)>,
        splitters: &mut Vec<Splitter>,
    ) {
        match self {
            Node::Leaf(g) => leaves.push((*g, rect)),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let half = gap / 2.0;
                let (a, b, line) = match axis {
                    Axis::Row => {
                        let x = (rect.min.x + rect.width() * ratio).round();
                        (
                            Rect::from_min_max(rect.min, Pos2::new(x - half, rect.max.y)),
                            Rect::from_min_max(Pos2::new(x + half, rect.min.y), rect.max),
                            Rect::from_min_max(
                                Pos2::new(x - half, rect.min.y),
                                Pos2::new(x + half, rect.max.y),
                            ),
                        )
                    }
                    Axis::Column => {
                        let y = (rect.min.y + rect.height() * ratio).round();
                        (
                            Rect::from_min_max(rect.min, Pos2::new(rect.max.x, y - half)),
                            Rect::from_min_max(Pos2::new(rect.min.x, y + half), rect.max),
                            Rect::from_min_max(
                                Pos2::new(rect.min.x, y - half),
                                Pos2::new(rect.max.x, y + half),
                            ),
                        )
                    }
                };
                splitters.push(Splitter {
                    path: path.clone(),
                    axis: *axis,
                    area: rect,
                    line,
                });
                path.push(0);
                first.layout(a, gap, path, leaves, splitters);
                path.pop();
                path.push(1);
                second.layout(b, gap, path, leaves, splitters);
                path.pop();
            }
        }
    }
}

/// All editor groups, their arrangement, and which one has focus.
pub struct Layout {
    pub groups: Vec<Group>,
    pub tree: Node,
    pub active: GroupId,
    next_id: u64,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            groups: vec![Group {
                id: 1,
                tabs: Vec::new(),
                active: 0,
            }],
            tree: Node::Leaf(1),
            active: 1,
            next_id: 2,
        }
    }
}

impl Layout {
    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn group(&self, id: GroupId) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn group_mut(&mut self, id: GroupId) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    pub fn active_group(&self) -> &Group {
        self.group(self.active).expect("active group exists")
    }

    pub fn active_group_mut(&mut self) -> &mut Group {
        let id = self.active;
        self.group_mut(id).expect("active group exists")
    }

    pub fn active_tab(&self) -> Option<Tab> {
        self.active_group().active_tab()
    }

    /// Groups in reading order (left to right, top to bottom).
    pub fn ordered(&self) -> Vec<GroupId> {
        let mut v = Vec::new();
        self.tree.order(&mut v);
        v
    }

    /// Number of tabs showing `doc`, across all groups.
    pub fn views_of(&self, doc: u64) -> usize {
        self.groups
            .iter()
            .flat_map(|g| &g.tabs)
            .filter(|t| t.doc == doc)
            .count()
    }

    /// The group holding `view` and its index there.
    pub fn find_view(&self, view: ViewId) -> Option<(GroupId, usize)> {
        self.groups
            .iter()
            .find_map(|g| Some((g.id, g.position(view)?)))
    }

    /// Show `doc` in the active group: switch to its tab there, or open a new
    /// one next to the active tab. Returns the tab.
    pub fn open(&mut self, doc: u64) -> Tab {
        let group = self.active_group();
        if let Some(i) = group.tabs.iter().position(|t| t.doc == doc) {
            self.active_group_mut().active = i;
            return self.active_group().tabs[i];
        }
        let tab = Tab {
            view: self.new_id(),
            doc,
        };
        self.active_group_mut().insert(tab);
        tab
    }

    /// Switch to the tab showing `doc`, preferring the active group. Returns
    /// whether there was one.
    pub fn reveal(&mut self, doc: u64) -> bool {
        let mut order = self.ordered();
        order.retain(|&g| g != self.active);
        order.insert(0, self.active);
        for g in order {
            let group = self.group_mut(g).expect("group exists");
            if let Some(i) = group.tabs.iter().position(|t| t.doc == doc) {
                group.active = i;
                self.active = g;
                return true;
            }
        }
        false
    }

    /// Split the active group, VSCode's "Split Editor": the new group opens
    /// on `side` with the active document (as a new view) and gets focus.
    pub fn split(&mut self, side: Side) -> GroupId {
        let id = self.new_id();
        let tabs: Vec<Tab> = match self.active_tab() {
            Some(t) => vec![Tab {
                view: self.new_id(),
                doc: t.doc,
            }],
            None => Vec::new(),
        };
        self.tree.split(self.active, id, side);
        self.groups.push(Group {
            id,
            tabs,
            active: 0,
        });
        self.active = id;
        id
    }

    /// Remove an empty group from the layout (the last group always stays).
    fn remove_group(&mut self, id: GroupId) {
        if self.groups.len() <= 1 {
            return;
        }
        if let Some(grows) = self.tree.remove(id) {
            self.groups.retain(|g| g.id != id);
            if self.active == id {
                self.active = grows;
            }
        }
    }

    /// Close a tab. A group left empty is removed, like VSCode does.
    pub fn close_tab(&mut self, view: ViewId) -> Option<Tab> {
        let (g, i) = self.find_view(view)?;
        let group = self.group_mut(g)?;
        let tab = group.remove(i);
        if group.tabs.is_empty() {
            self.remove_group(g);
        }
        Some(tab)
    }

    /// Close the active group if it's empty and not the only one.
    pub fn close_empty_active_group(&mut self) -> bool {
        if self.active_group().tabs.is_empty() && self.groups.len() > 1 {
            self.remove_group(self.active);
            return true;
        }
        false
    }

    /// Move a tab into group `to`, or with `side`, into a new group split off
    /// on that side of `to`. If the target already shows the document, its
    /// existing tab is used and the moved view is returned as dropped.
    pub fn move_tab(&mut self, view: ViewId, to: GroupId, side: Option<Side>) -> Option<ViewId> {
        let (from, idx) = self.find_view(view)?;
        self.group(to)?;
        let target = match side {
            // Splitting a group off its only tab would leave it empty.
            Some(_) if from == to && self.group(from)?.tabs.len() == 1 => return None,
            Some(side) => {
                let id = self.new_id();
                self.tree.split(to, id, side);
                self.groups.push(Group {
                    id,
                    tabs: Vec::new(),
                    active: 0,
                });
                id
            }
            None if from == to => return None,
            None => to,
        };
        let tab = self.group_mut(from)?.remove(idx);
        let dest = self.group_mut(target)?;
        let dropped = match dest.tabs.iter().position(|t| t.doc == tab.doc) {
            Some(i) => {
                dest.active = i;
                Some(tab.view)
            }
            None => {
                dest.insert(tab);
                None
            }
        };
        self.active = target;
        if self.group(from)?.tabs.is_empty() {
            self.remove_group(from);
        }
        dropped
    }

    /// Merge every group into one, VSCode's "Single" editor layout. Returns
    /// the views dropped because their document was already in the merged
    /// group.
    pub fn join_all(&mut self) -> Vec<ViewId> {
        let order = self.ordered();
        let active_tab = self.active_tab();
        let mut tabs: Vec<Tab> = Vec::new();
        let mut dropped = Vec::new();
        for g in &order {
            for t in &self.group(*g).expect("group exists").tabs {
                if tabs.iter().any(|x| x.doc == t.doc) {
                    dropped.push(t.view);
                } else {
                    tabs.push(*t);
                }
            }
        }
        let keep = order[0];
        let active = active_tab
            .and_then(|a| tabs.iter().position(|t| t.doc == a.doc))
            .unwrap_or(0);
        self.groups = vec![Group {
            id: keep,
            tabs,
            active,
        }];
        self.tree = Node::Leaf(keep);
        self.active = keep;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::vec2;

    fn docs(l: &Layout, g: GroupId) -> Vec<u64> {
        l.group(g).unwrap().tabs.iter().map(|t| t.doc).collect()
    }

    #[test]
    fn split_copies_the_active_document() {
        let mut l = Layout::default();
        let a = l.open(10);
        l.open(11);
        l.active_group_mut().active = 0;
        let g1 = l.active;
        let g2 = l.split(Side::Right);
        assert_eq!(l.active, g2);
        assert_eq!(docs(&l, g2), vec![10]);
        assert_ne!(l.active_tab().unwrap().view, a.view, "a new view");
        assert_eq!(l.ordered(), vec![g1, g2]);
        let g3 = l.split(Side::Up);
        assert_eq!(l.ordered(), vec![g1, g3, g2]);
        assert_eq!(l.views_of(10), 3);
    }

    #[test]
    fn closing_the_last_tab_removes_the_group() {
        let mut l = Layout::default();
        l.open(10);
        let g1 = l.active;
        let g2 = l.split(Side::Down);
        let t = l.active_tab().unwrap();
        l.close_tab(t.view);
        assert_eq!(l.groups.len(), 1);
        assert_eq!(l.active, g1);
        assert!(matches!(l.tree, Node::Leaf(g) if g == g1));
        assert!(l.group(g2).is_none());
        // The last group stays, even when empty.
        let t = l.active_tab().unwrap();
        l.close_tab(t.view);
        assert_eq!(l.groups.len(), 1);
        assert!(l.active_tab().is_none());
    }

    #[test]
    fn removing_a_nested_group_promotes_its_sibling() {
        let mut l = Layout::default();
        l.open(1);
        let g1 = l.active;
        let g2 = l.split(Side::Right);
        let g3 = l.split(Side::Down); // splits g2
        l.close_tab(l.active_tab().unwrap().view); // closes g3
        assert_eq!(l.ordered(), vec![g1, g2]);
        assert_eq!(l.active, g2);
        assert!(l.group(g3).is_none());
    }

    #[test]
    fn move_tab_between_groups_and_into_new_splits() {
        let mut l = Layout::default();
        let a = l.open(1);
        l.open(2);
        let g1 = l.active;
        let g2 = l.split(Side::Right); // shows doc 2
        assert_eq!(l.move_tab(a.view, g2, None), None);
        assert_eq!(docs(&l, g1), vec![2]);
        assert_eq!(docs(&l, g2), vec![2, 1]);
        assert_eq!(l.active, g2);

        // Moving doc 2 from g1 onto g2, which already shows it, drops the view
        // and removes the now empty g1.
        let b = l.group(g1).unwrap().tabs[0];
        assert_eq!(l.move_tab(b.view, g2, None), Some(b.view));
        assert_eq!(l.ordered(), vec![g2]);

        // Dragging a tab onto its own group's edge splits it off.
        assert_eq!(l.move_tab(a.view, g2, Some(Side::Down)), None);
        let order = l.ordered();
        assert_eq!(order.len(), 2);
        assert_eq!(docs(&l, order[1]), vec![1]);
        // But not if it's the group's only tab.
        assert_eq!(l.move_tab(a.view, order[1], Some(Side::Left)), None);
        assert_eq!(l.ordered(), order);
    }

    #[test]
    fn join_all_merges_without_duplicates() {
        let mut l = Layout::default();
        l.open(1);
        l.split(Side::Right);
        l.open(2);
        l.split(Side::Down);
        let dropped = l.join_all();
        assert_eq!(dropped.len(), 2);
        assert_eq!(l.groups.len(), 1);
        assert_eq!(docs(&l, l.active), vec![1, 2]);
        assert_eq!(l.active_tab().unwrap().doc, 2);
    }

    #[test]
    fn layout_rects() {
        let mut l = Layout::default();
        l.open(1);
        let g1 = l.active;
        let g2 = l.split(Side::Right);
        let g3 = l.split(Side::Down);
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0));
        let (mut leaves, mut splitters) = (Vec::new(), Vec::new());
        l.tree
            .layout(rect, 2.0, &mut Vec::new(), &mut leaves, &mut splitters);
        assert_eq!(
            leaves[0],
            (g1, Rect::from_min_max(Pos2::ZERO, Pos2::new(99.0, 100.0)))
        );
        assert_eq!(
            leaves[1],
            (
                g2,
                Rect::from_min_max(Pos2::new(101.0, 0.0), Pos2::new(200.0, 49.0))
            )
        );
        assert_eq!(leaves[2].0, g3);
        assert_eq!(splitters.len(), 2);
        assert_eq!(splitters[1].path, vec![1]);
        assert!(l.tree.at_path_mut(&[1]).is_some());
        assert!(l.tree.at_path_mut(&[0, 0]).is_none());
    }
}
