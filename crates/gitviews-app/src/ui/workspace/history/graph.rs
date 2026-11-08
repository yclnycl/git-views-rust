//! Continuous topology lanes. Page boundaries retain the same pending parents.
use gitviews_service::GraphCommit;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Segment {
    pub from: usize,
    pub to: usize,
    pub starts_here: bool,
    pub color: usize,
}

#[derive(Clone, Debug)]
pub(super) struct GraphRow {
    pub commit: GraphCommit,
    pub matches: bool,
    pub lane: usize,
    pub color: usize,
    pub incoming: bool,
    pub segments: Vec<Segment>,
    pub lanes: usize,
}

#[derive(Default)]
pub(super) struct Layout {
    lanes: Vec<(String, usize)>,
    next_color: usize,
}

impl Layout {
    fn color(&mut self) -> usize {
        let color = self.next_color;
        self.next_color = self.next_color.wrapping_add(1);
        color
    }

    pub fn push(&mut self, commit: GraphCommit, matches: bool) -> GraphRow {
        let found = self.lanes.iter().position(|(id, _)| id == &commit.id);
        let incoming = found.is_some();
        let lane = found.unwrap_or_else(|| {
            let color = self.color();
            self.lanes.push((commit.id.clone(), color));
            self.lanes.len() - 1
        });
        let color = self.lanes[lane].1;
        let before = self.lanes.clone();
        self.lanes.remove(lane);
        for (index, parent) in commit.parents.iter().enumerate() {
            if !self.lanes.iter().any(|(id, _)| id == parent) {
                let parent_color = if index == 0 { color } else { self.color() };
                self.lanes.insert(
                    (lane + index).min(self.lanes.len()),
                    (parent.clone(), parent_color),
                );
            }
        }
        let mut segments = before
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != lane)
            .filter_map(|(index, (id, color))| {
                self.lanes
                    .iter()
                    .position(|(next, _)| next == id)
                    .map(|next| Segment {
                        from: index,
                        to: next,
                        starts_here: false,
                        color: *color,
                    })
            })
            .collect::<Vec<_>>();
        for parent in &commit.parents {
            if let Some(next) = self.lanes.iter().position(|(id, _)| id == parent) {
                segments.push(Segment {
                    from: lane,
                    to: next,
                    starts_here: true,
                    color: self.lanes[next].1,
                });
            }
        }
        GraphRow {
            commit,
            matches,
            lane,
            color,
            incoming,
            segments,
            lanes: before.len().max(self.lanes.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn commit(id: &str, parents: &[&str]) -> GraphCommit {
        GraphCommit {
            id: id.into(),
            parents: parents.iter().map(|p| (*p).into()).collect(),
            subject: id.into(),
            author: "作者".into(),
            time: 0,
        }
    }
    #[test]
    fn merge_lanes_keep_colors_and_connect_continuously_across_pages() {
        let mut layout = Layout::default();
        let merge = layout.push(commit("merge", &["left", "right"]), true);
        assert_eq!(merge.segments.len(), 2);
        let branch = layout.push(commit("right", &["base"]), false);
        assert!(branch.incoming);
        assert_ne!(branch.color, merge.color);
        let left = layout.push(commit("left", &["base"]), true);
        assert!(left.segments.iter().any(|edge| edge.starts_here));
        let base = layout.push(commit("base", &[]), true);
        assert!(base.incoming);
        assert!(layout.lanes.is_empty());
        assert!(!branch.matches);
    }
    #[test]
    fn unrelated_tips_and_octopus_parents_do_not_collapse_into_one_lane() {
        let mut layout = Layout::default();
        let first = layout.push(commit("tip", &["a", "b", "c"]), true);
        assert_eq!(first.lanes, 3);
        let unrelated = layout.push(commit("other", &["root"]), true);
        assert!(!unrelated.incoming);
        assert_eq!(unrelated.lane, 3);
        assert_eq!(unrelated.segments.len(), 4);
        let row = layout.push(commit("b", &["root"]), true);
        assert!(row.incoming);
        assert_eq!(row.lanes, 4);
    }
}
