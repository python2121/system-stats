// Commit-graph lane layout, in the style of VS Code's "Git Graph"
// extension: one row per commit, stable columns (a branch keeps its lane
// until it ends; freed lanes are reused by later branches), and rounded
// elbows where branches fork off and merge in.
//
// The engine walks commits top-down — children before parents, the order
// `git log --date-order` guarantees — holding a list of active lanes,
// each recording the sha it expects to see next:
//
//   ● commit row: the leftmost lane expecting this sha gets the node.
//     Other lanes expecting it are branches that forked here — they curve
//     into the node (`●─╯`) and are freed for reuse.
//   ● first parent: the node's lane simply starts expecting it.
//   ● extra parents (merges): each either curves out to a fresh lane
//     (`◉─╮`) or joins an existing lane already expecting that parent
//     (`├─◉`).
//
// Every cell is a set of connection bits (up/down/left/right + node), and
// the bit-combination maps onto one box-drawing glyph — so junctions of
// any complexity (a merge line crossing an unrelated lane → `┼`, two
// forks collapsing through each other → `┴`) fall out of one table
// instead of special cases.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::git::GraphRow;

// Palette borrowed from VS Code's Git Graph / GitHub's PR graph: blue
// leads (so `main` in lane 0 gets the trunk color), then bright,
// well-separated hues that read clearly on dark and light backgrounds.
const LANE_COLORS: [Color; 6] = [
    Color::Rgb(88, 166, 255),  // blue    — #58A6FF (trunk)
    Color::Rgb(247, 120, 186), // pink    — #F778BA
    Color::Rgb(126, 231, 135), // green   — #7EE787
    Color::Rgb(240, 184, 74),  // amber   — #F0B84A
    Color::Rgb(163, 113, 247), // purple  — #A371F7
    Color::Rgb(255, 122, 89),  // orange  — #FF7A59
];

fn lane_color(lane: usize) -> Color {
    LANE_COLORS[lane % LANE_COLORS.len()]
}

#[derive(Clone, Copy, Default)]
struct Cell {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    // Commit disc sits here — wins over the connection bits for glyph
    // choice; `merge` picks the ring variant, `ghost` the hollow one
    // (the last commit of a merged-and-deleted branch).
    node: bool,
    merge: bool,
    ghost: bool,
    // Palette slot that owns this cell's color. Verticals claim their
    // cell first, so a horizontal run crossing a rail leaves the rail's
    // color intact (`┼` keeps the through-lane's hue) while pure
    // horizontal cells read as the curving branch.
    color: Option<usize>,
}

// A lane holds the sha it expects to see next and the palette slot its
// branch was assigned at birth. Color travels with the branch, not the
// column (VS Code Git Graph's rule): a lane freed and later reused by a
// different branch gets a fresh color, so neighbours stay distinct.
struct Lane {
    expects: String,
    color: usize,
    // Opened by a merge's extra parent. When the expected commit lands
    // on this lane bearing no ref, it's the final commit of a branch
    // that was merged and then deleted — drawn hollow (○).
    merge_born: bool,
}

// Least-used palette slot among the live lanes, lowest index winning
// ties — so the trunk keeps blue for its whole life and freed colors
// are recycled before any hue has to double up. While lane 0 is still
// reserved for an upcoming trunk commit, blue is reserved with it.
fn alloc_color(lanes: &[Option<Lane>], trunk_pending: bool) -> usize {
    let mut used = [0usize; LANE_COLORS.len()];
    if trunk_pending {
        used[0] += 1;
    }
    for lane in lanes.iter().flatten() {
        used[lane.color % LANE_COLORS.len()] += 1;
    }
    (0..LANE_COLORS.len())
        .min_by_key(|&c| (used[c], c))
        .unwrap_or(0)
}

// Is this decoration the repo's trunk? `origin/HEAD` marks the remote
// default branch whatever its name; main/master cover the common cases
// (and local-only repos).
fn is_trunk_ref(r: &str) -> bool {
    let r = r.strip_prefix("HEAD -> ").unwrap_or(r);
    matches!(
        r,
        "main" | "master" | "origin/main" | "origin/master" | "origin/HEAD"
    )
}

impl Cell {
    fn add(&mut self, up: bool, down: bool, left: bool, right: bool, color: usize) {
        // A lane's vertical (rail, elbow, junction) owns its cell's color;
        // horizontal runs only color cells nothing vertical touches.
        // Without this, a fork's long horizontal painted before a merge
        // lane is born leaves the newborn's elbow in the wrong hue, and
        // its rail changes color one row down.
        let had_vertical = self.up || self.down;
        if self.color.is_none() || ((up || down) && !had_vertical) {
            self.color = Some(color);
        }
        self.up |= up;
        self.down |= down;
        self.left |= left;
        self.right |= right;
    }

    fn glyph(&self) -> char {
        if self.node {
            return if self.merge {
                '◉'
            } else if self.ghost {
                '○'
            } else {
                '●'
            };
        }
        match (self.up, self.down, self.left, self.right) {
            (true, true, false, false) => '│',
            (false, false, true, true) => '─',
            (true, true, true, true) => '┼',
            (true, false, true, false) => '╯',
            (true, false, false, true) => '╰',
            (false, true, true, false) => '╮',
            (false, true, false, true) => '╭',
            (true, true, true, false) => '┤',
            (true, true, false, true) => '├',
            (false, true, true, true) => '┬',
            (true, false, true, true) => '┴',
            // Degenerate stubs (single bit) shouldn't occur, but render
            // as something sensible rather than panicking.
            (true, false, false, false) | (false, true, false, false) => '│',
            (false, false, true, false) | (false, false, false, true) => '─',
            (false, false, false, false) => ' ',
        }
    }
}

fn grow(cells: &mut Vec<Cell>, idx: usize) {
    if cells.len() <= idx {
        cells.resize(idx + 1, Cell::default());
    }
}

pub struct Layout {
    // One cell-row per commit, parallel to the input slice. Lane `i`
    // lives at cell index `2*i`; odd indices are the gaps between lanes
    // (used by horizontal merge/fork runs).
    rows: Vec<Vec<Cell>>,
    // Widest row in cells — rows are padded to this so the text to the
    // right of the graph lines up in a column.
    pub width: usize,
}

pub fn layout(commits: &[GraphRow]) -> Layout {
    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(commits.len());
    let mut width = 0usize;

    // Pin the trunk to lane 0 (à la jj reserving a column for `@`): if a
    // trunk-decorated commit is anywhere in the window, lane 0 and the
    // blue palette slot are held for it. A checked-out branch that's
    // simply ahead of main then renders as a branch hanging off the
    // trunk (`  ●` above `●─╯`) instead of an unbroken chain — even
    // though the history hasn't diverged yet.
    let trunk_sha: Option<&str> = commits
        .iter()
        .find(|c| c.refs.iter().any(|r| is_trunk_ref(r)))
        .map(|c| c.sha.as_str());
    let mut trunk_pending = trunk_sha.is_some();

    for commit in commits {
        if trunk_pending && lanes.is_empty() {
            lanes.push(None); // hold the reserved column open
        }
        let mut cells: Vec<Cell> = vec![Cell::default(); 2 * lanes.len().max(1)];

        // Lanes whose next expected commit is this one. The leftmost gets
        // the node; the rest are forks collapsing into it.
        let matches: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_ref().is_some_and(|l| l.expects == commit.sha))
            .map(|(i, _)| i)
            .collect();

        // Lanes freed on this row can't be reallocated until the next row
        // — their column is already drawing this branch's closing curve.
        let mut freed: Vec<usize> = Vec::new();
        // Lanes born on this row have no rail above yet, so a second
        // connection into them must not set the `up` bit.
        let mut born: Vec<usize> = Vec::new();

        let is_trunk_row = trunk_pending && Some(commit.sha.as_str()) == trunk_sha;
        if is_trunk_row {
            trunk_pending = false;
        }
        let reserve0 = trunk_pending;

        // Lowest free column — skipping lane 0 while it's held for the
        // trunk — or a newly opened one on the right.
        let alloc_lane = |lanes: &mut Vec<Option<Lane>>, skip: &[usize]| -> usize {
            let free = lanes.iter().enumerate().position(|(i, l)| {
                l.is_none() && !skip.contains(&i) && !(reserve0 && i == 0)
            });
            free.unwrap_or_else(|| {
                lanes.push(None);
                lanes.len() - 1
            })
        };

        // On the trunk's row, EVERY lane expecting it collapses into the
        // reserved column — the node doesn't inherit any of their lanes.
        let (node_lane, node_color, collapsing): (usize, usize, &[usize]) = if is_trunk_row {
            born.push(0);
            (0, 0, &matches[..])
        } else if let Some(&i) = matches.first() {
            (i, lanes[i].as_ref().map_or(0, |l| l.color), &matches[1..])
        } else {
            // Tip of a branch nothing expected — take the lowest free
            // column, or open a new one, and start a fresh color.
            let i = alloc_lane(&mut lanes, &[]);
            born.push(i);
            (i, alloc_color(&lanes, reserve0), &[])
        };

        // Pass-through rails for every uninvolved active lane.
        for (i, lane) in lanes.iter().enumerate() {
            if i == node_lane || matches.contains(&i) {
                continue;
            }
            if let Some(lane) = lane {
                grow(&mut cells, 2 * i);
                cells[2 * i].add(true, true, false, false, lane.color);
            }
        }

        // The node itself. `up` iff a rail was already flowing down the
        // node's own lane (never true on the trunk's reserved row);
        // `down` is added below once we know it has a parent.
        grow(&mut cells, 2 * node_lane);
        cells[2 * node_lane].node = true;
        cells[2 * node_lane].merge = commit.parents.len() > 1;
        cells[2 * node_lane].ghost = lanes[node_lane]
            .as_ref()
            .is_some_and(|l| l.merge_born)
            && commit.refs.is_empty();
        cells[2 * node_lane].color = Some(node_color);
        cells[2 * node_lane].up = matches.contains(&node_lane);

        // A horizontal run from the node to lane `k`, colored as the
        // branch doing the curving. Only fills the strictly in-between
        // cells; the endpoints add their own bits.
        let run = |cells: &mut Vec<Cell>, k: usize, color: usize| {
            let (a, b) = (2 * node_lane.min(k) + 1, 2 * node_lane.max(k) - 1);
            grow(cells, b);
            for cell in &mut cells[a..=b] {
                cell.add(false, false, true, true, color);
            }
            if k > node_lane {
                cells[2 * node_lane].right = true;
            } else {
                cells[2 * node_lane].left = true;
            }
        };

        // Forks collapsing: every other lane expecting this commit curves
        // into the node and dies.  ╯ from the right, ╰ from the left.
        for &j in collapsing {
            let color = lanes[j].as_ref().map_or(0, |l| l.color);
            run(&mut cells, j, color);
            grow(&mut cells, 2 * j);
            cells[2 * j].add(true, false, j > node_lane, j < node_lane, color);
            freed.push(j);
        }

        // First parent flows straight down the node's lane; a root commit
        // ends its lane here instead.
        match commit.parents.first() {
            Some(p) => {
                lanes[node_lane] = Some(Lane {
                    expects: p.clone(),
                    color: node_color,
                    merge_born: false,
                });
                cells[2 * node_lane].down = true;
            }
            None => freed.push(node_lane),
        }

        // Extra parents: merge lines curving out. Join a lane that
        // already expects this parent (├ / ┤) in that branch's color,
        // else open a fresh lane — and a fresh color — with a rounded
        // elbow (╭ / ╮).
        for p in commit.parents.iter().skip(1) {
            let existing = lanes
                .iter()
                .position(|l| l.as_ref().is_some_and(|l| &l.expects == p))
                .filter(|&k| k != node_lane);
            let (k, color) = match existing {
                Some(k) => (k, lanes[k].as_ref().map_or(0, |l| l.color)),
                None => {
                    let k = alloc_lane(&mut lanes, &freed);
                    let color = alloc_color(&lanes, reserve0);
                    lanes[k] = Some(Lane {
                        expects: p.clone(),
                        color,
                        merge_born: true,
                    });
                    born.push(k);
                    (k, color)
                }
            };
            run(&mut cells, k, color);
            grow(&mut cells, 2 * k);
            cells[2 * k].add(
                !born.contains(&k),
                true,
                k > node_lane,
                k < node_lane,
                color,
            );
        }

        // Now that allocation is done, actually release the lanes that
        // ended on this row and drop unused columns off the right edge.
        for j in freed {
            lanes[j] = None;
        }
        while lanes.last().is_some_and(|l| l.is_none()) {
            lanes.pop();
        }

        // Trim trailing empties so `width` reflects real content.
        while cells
            .last()
            .is_some_and(|c| !c.node && c.glyph() == ' ')
        {
            cells.pop();
        }
        width = width.max(cells.len());
        rows.push(cells);
    }

    Layout { rows, width }
}

// Colored, padded spans for one row's graph columns. Commit discs are
// bold so they pop above the rails without changing width.
pub fn prefix_spans(layout: &Layout, idx: usize) -> Vec<Span<'static>> {
    let cells = &layout.rows[idx];
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(layout.width);
    for cell in cells {
        let g = cell.glyph();
        if g == ' ' {
            spans.push(Span::raw(" "));
            continue;
        }
        let mut style = Style::default().fg(lane_color(cell.color.unwrap_or(0)));
        if cell.node {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(g.to_string(), style));
    }
    if cells.len() < layout.width {
        spans.push(Span::raw(" ".repeat(layout.width - cells.len())));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> GraphRow {
        GraphRow {
            sha: sha.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            timestamp: None,
            date: String::new(),
            author: String::new(),
            subject: String::new(),
            refs: Vec::new(),
        }
    }

    fn render(commits: &[GraphRow]) -> Vec<String> {
        let l = layout(commits);
        l.rows
            .iter()
            .map(|cells| cells.iter().map(Cell::glyph).collect())
            .collect()
    }

    fn commit_refs(sha: &str, parents: &[&str], refs: &[&str]) -> GraphRow {
        GraphRow {
            refs: refs.iter().map(|s| s.to_string()).collect(),
            ..commit(sha, parents)
        }
    }

    #[test]
    fn branch_ahead_of_main_hangs_off_the_trunk() {
        // A checked-out branch one commit ahead of main, zero divergence
        // — visually a branch off the trunk, not an unbroken chain.
        let commits = [
            commit_refs("b1", &["m1"], &["HEAD -> feat", "origin/feat"]),
            commit_refs("m1", &["m0"], &["origin/main", "origin/HEAD", "main"]),
            commit("m0", &[]),
        ];
        let l = layout(&commits);
        let glyphs: Vec<String> = l
            .rows
            .iter()
            .map(|cells| cells.iter().map(Cell::glyph).collect())
            .collect();
        assert_eq!(glyphs, vec!["  ●", "●─╯", "●"]);
        // Trunk wears blue even though the branch tip rendered first.
        assert_eq!(l.rows[1][0].color, Some(0));
        assert_eq!(l.rows[0][2].color, Some(1));
        // The collapse elbow is the branch's hue, not the trunk's.
        assert_eq!(l.rows[1][2].color, Some(1));
    }

    #[test]
    fn on_main_stays_a_straight_line() {
        // HEAD on main itself: reservation resolves on row 0, no phantom
        // lane appears.
        let commits = [
            commit_refs("m1", &["m0"], &["HEAD -> main", "origin/main"]),
            commit("m0", &[]),
        ];
        let l = layout(&commits);
        let glyphs: Vec<String> = l
            .rows
            .iter()
            .map(|cells| cells.iter().map(Cell::glyph).collect())
            .collect();
        assert_eq!(glyphs, vec!["●", "●"]);
    }

    #[test]
    fn linear_history() {
        let rows = render(&[
            commit("c", &["b"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows, vec!["●", "●", "●"]);
    }

    #[test]
    fn fork_collapses_with_rounded_elbow() {
        // C (main) and B (feature) both branched from A.
        let rows = render(&[
            commit("c", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows, vec!["●", "│ ●", "●─╯"]);
    }

    #[test]
    fn merge_opens_lane_and_closes_it() {
        // M merges feature (B) into main (C); both stem from A. B has no
        // surviving ref, so it draws hollow: the tip of a branch that
        // was merged and then deleted.
        let rows = render(&[
            commit("m", &["c", "b"]),
            commit("c", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows, vec!["◉─╮", "● │", "│ ○", "●─╯"]);
    }

    #[test]
    fn surviving_branch_tip_stays_solid() {
        // Same shape, but B's branch ref still exists — solid node.
        let rows = render(&[
            commit("m", &["c", "b"]),
            commit("c", &["a"]),
            commit_refs("b", &["a"], &["origin/feat/x"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows[2], "│ ●");
    }

    #[test]
    fn only_the_deleted_tip_is_hollow() {
        // A two-commit deleted branch: the tip is ○, the commit below it
        // on the same lane is a normal ●.
        let rows = render(&[
            commit("m", &["c", "b2"]),
            commit("c", &["a"]),
            commit("b2", &["b1"]),
            commit("b1", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows, vec!["◉─╮", "● │", "│ ○", "│ ●", "●─╯"]);
    }

    #[test]
    fn merge_into_existing_lane_uses_tee() {
        // C sits above merge M; M's second parent B is already expected
        // by C's lane, so the merge line tees into the existing rail.
        let rows = render(&[
            commit("c", &["b"]),
            commit("m", &["a", "b"]),
            commit("b", &[]),
            commit("a", &[]),
        ]);
        assert_eq!(rows, vec!["●", "├─◉", "● │", "  ●"]);
    }

    #[test]
    fn merge_line_crosses_unrelated_lane() {
        // T expects M in lane 0, U holds lane 1; M's second parent opens
        // lane 2, so its merge line crosses U's rail as ┼.
        let rows = render(&[
            commit("t", &["m"]),
            commit("u", &["b"]),
            commit("m", &["a", "x"]),
            commit("x", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ]);
        assert_eq!(rows[0], "●");
        assert_eq!(rows[1], "│ ●");
        assert_eq!(rows[2], "◉─┼─╮");
    }

    #[test]
    fn color_follows_branch_not_column() {
        // Two branches live concurrently → distinct colors. After the
        // first dies, a NEW branch reusing its column gets a fresh
        // allocation (which may recycle the freed slot — but never while
        // the old branch is still on screen).
        let commits = [
            commit("m", &["c", "b"]),
            commit("c", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ];
        let l = layout(&commits);
        let trunk = l.rows[0][0].color;
        let branch = l.rows[0][2].color; // the ╮ elbow
        assert_eq!(trunk, Some(0));
        assert_eq!(branch, Some(1));
        // The collapse elbow on a's row still wears the branch's color.
        assert_eq!(l.rows[3][2].color, Some(1));
        // And the horizontal run cells match their curving branch.
        assert_eq!(l.rows[0][1].color, Some(1));
    }

    #[test]
    fn newborn_elbow_keeps_its_branch_color() {
        // A fork's horizontal run is painted before the merge lane is
        // born in a column it crosses — the newborn's ┬ elbow must wear
        // the new branch's color, not the run's (seen live in
        // claude-usage's "Merge PR #28" row).
        let commits = [
            commit("t0", &["m"]),   // lane 0
            commit("t1", &["z"]),   // lane 1 — stays busy, gets crossed
            commit("t2", &["q"]),   // lane 2 — will free up before the merge
            commit("t3", &["m"]),   // lane 3 — collapses across everything
            commit("q", &[]),       // root: frees lane 2 (lane 3 keeps it un-trimmed)
            commit("m", &["w", "v"]),
        ];
        let l = layout(&commits);
        assert_eq!(
            l.rows[5].iter().map(Cell::glyph).collect::<String>(),
            "◉─┼─┬─╯",
        );
        // ┼ keeps the crossed rail's color; ┬ belongs to the newborn lane.
        assert_eq!(l.rows[5][2].color, Some(1));
        let newborn = l.rows[5][4].color;
        assert_ne!(newborn, l.rows[5][6].color, "┬ must not copy the ╯'s hue");
        // And the elbow matches the rail it feeds on the next row.
        let commits: Vec<GraphRow> = commits
            .into_iter()
            .chain([commit("k", &["z2"])])
            .collect();
        let l = layout(&commits);
        assert_eq!(l.rows[6][4].color, newborn);
    }

    #[test]
    fn freed_lane_is_reused() {
        // feature merges away, then a later branch reuses its column.
        let rows = render(&[
            commit("m", &["c", "b"]),
            commit("c", &["a2"]),
            commit("b", &["a2"]),
            commit("d", &["a2"]),
            commit("a2", &["a1"]),
            commit("a1", &[]),
        ]);
        // b dies into a2's row eventually; d (a fresh tip) should sit in
        // a low column, not open lane 3.
        assert!(rows.iter().all(|r| r.chars().count() <= 5));
    }

    // Dev harness, not a test: renders a real repo's graph to stdout so
    // layout changes can be eyeballed against `git log --graph`.
    //   GRAPH_REPO=~/some/repo cargo test dump_real_repo -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_real_repo() {
        let Ok(repo) = std::env::var("GRAPH_REPO") else {
            return;
        };
        let commits = crate::git::graph(std::path::Path::new(&repo));
        let l = layout(&commits);
        for (i, row) in commits.iter().enumerate() {
            let cells: String = l.rows[i]
                .iter()
                .map(|c| {
                    let Color::Rgb(r, g, b) = lane_color(c.color.unwrap_or(0)) else {
                        return c.glyph().to_string();
                    };
                    format!("\x1b[38;2;{r};{g};{b}m{}\x1b[0m", c.glyph())
                })
                .collect();
            let pad = " ".repeat(l.width.saturating_sub(l.rows[i].len()));
            let refs = if row.refs.is_empty() {
                String::new()
            } else {
                format!(" ({})", row.refs.join(", "))
            };
            println!(
                "{cells}{pad} {}{refs} {}",
                &row.sha[..7.min(row.sha.len())],
                row.subject
            );
        }
    }

    #[test]
    fn root_commit_ends_lane() {
        let rows = render(&[commit("b", &["a"]), commit("a", &[])]);
        assert_eq!(rows, vec!["●", "●"]);
        // And nothing rails below a root: re-laying out with a following
        // unrelated tip must reuse lane 0.
        let rows = render(&[
            commit("b", &["a"]),
            commit("a", &[]),
            commit("z", &[]),
        ]);
        assert_eq!(rows, vec!["●", "●", "●"]);
    }

    #[test]
    fn trunk_refs_are_recognized_in_every_decoration_form() {
        assert!(is_trunk_ref("main"));
        assert!(is_trunk_ref("master"));
        assert!(is_trunk_ref("origin/main"));
        assert!(is_trunk_ref("origin/HEAD"));
        // The checked-out form carries a "HEAD -> " prefix.
        assert!(is_trunk_ref("HEAD -> main"));
        assert!(is_trunk_ref("HEAD -> origin/master"));
    }

    #[test]
    fn trunk_refs_dont_match_lookalike_branches() {
        assert!(!is_trunk_ref("mainline"));
        assert!(!is_trunk_ref("feature/main"));
        assert!(!is_trunk_ref("tag: v1.0"));
        assert!(!is_trunk_ref("upstream/main"));
        assert!(!is_trunk_ref(""));
    }

    #[test]
    fn an_empty_history_lays_out_to_nothing() {
        // A repo with no commits in the window must not panic the renderer.
        let layout = layout(&[]);
        assert_eq!(layout.width, 0);
        assert!(render(&[]).is_empty());
    }

    #[test]
    fn every_row_is_padded_to_the_same_width() {
        // Ragged rows would leave the commit text jagged down the pane.
        let commits = vec![
            commit("m", &["a", "b"]),
            commit("a", &["base"]),
            commit("b", &["base"]),
            commit("base", &[]),
        ];
        let layout = layout(&commits);
        for idx in 0..commits.len() {
            let width: usize = prefix_spans(&layout, idx)
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            assert_eq!(width, layout.width, "row {idx} is ragged");
        }
    }

    #[test]
    fn commit_discs_are_bold_and_rails_are_not() {
        let layout = layout(&[commit("a", &["b"]), commit("b", &[])]);
        let bold: Vec<String> = prefix_spans(&layout, 0)
            .into_iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.into_owned())
            .collect();
        assert_eq!(bold, ["●"]);
    }

    #[test]
    fn lane_allocation_prefers_the_least_used_color() {
        // With nothing in flight the first slot wins; with the trunk slot
        // held, a new branch takes a different color.
        assert_eq!(alloc_color(&[], false), 0);
        assert_ne!(alloc_color(&[], true), 0);
    }
}
