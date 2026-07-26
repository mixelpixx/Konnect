//! Deterministic traversal of KiCad's three-dimensional schematic R-tree.
//!
//! KiCad inserts schematic items into an eight-branch Guttman R-tree and then
//! uses its depth-first traversal as the tie order for items with equal type
//! and layer. The first dimension is the schematic item type; the remaining
//! dimensions are integer schematic coordinates.

use crate::native_scene::Bounds;

const MAX_BRANCHES: usize = 8;
const MIN_BRANCHES: usize = MAX_BRANCHES / 2;

#[derive(Clone, Copy)]
struct Rect {
    min_type: f64,
    min_x: f64,
    min_y: f64,
    max_type: f64,
    max_x: f64,
    max_y: f64,
}

impl From<(i32, Bounds)> for Rect {
    fn from((item_type, bounds): (i32, Bounds)) -> Self {
        const IU_PER_MM: f64 = 10_000.0;
        Self {
            min_type: f64::from(item_type),
            min_x: (bounds.min_x * IU_PER_MM).round(),
            min_y: (bounds.min_y * IU_PER_MM).round(),
            max_type: f64::from(item_type),
            max_x: (bounds.max_x * IU_PER_MM).round(),
            max_y: (bounds.max_y * IU_PER_MM).round(),
        }
    }
}

impl Rect {
    fn combine(self, other: Self) -> Self {
        Self {
            min_type: self.min_type.min(other.min_type),
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_type: self.max_type.max(other.max_type),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    fn spherical_volume(self) -> f64 {
        let half_type = (self.max_type - self.min_type) * 0.5;
        let half_width = (self.max_x - self.min_x) * 0.5;
        let half_height = (self.max_y - self.min_y) * 0.5;
        let squared_radius =
            half_type * half_type + half_width * half_width + half_height * half_height;
        let radius = squared_radius.sqrt();
        radius * radius * radius * f64::from(4.188_79_f32)
    }

    fn overlaps(self, other: Self) -> bool {
        self.min_type <= other.max_type
            && self.max_type >= other.min_type
            && self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }
}

enum Payload {
    Item(usize),
    Child(Box<Node>),
}

struct Branch {
    rect: Rect,
    payload: Payload,
}

struct Node {
    level: usize,
    branches: Vec<Branch>,
}

impl Node {
    fn leaf() -> Self {
        Self {
            level: 0,
            branches: Vec::new(),
        }
    }

    fn cover(&self) -> Rect {
        self.branches
            .iter()
            .map(|branch| branch.rect)
            .reduce(Rect::combine)
            .expect("an indexed R-tree node is never empty")
    }

    fn add(&mut self, branch: Branch) -> Option<Self> {
        if self.branches.len() < MAX_BRANCHES {
            self.branches.push(branch);
            None
        } else {
            Some(self.split(branch))
        }
    }

    fn insert_branch(&mut self, branch: Branch, target_level: usize) -> Option<Self> {
        if self.level == target_level {
            return self.add(branch);
        }

        let branch_index = self.pick_branch(branch.rect);
        let other = {
            let Payload::Child(child) = &mut self.branches[branch_index].payload else {
                unreachable!("an internal R-tree branch always owns a child")
            };
            child.insert_branch(branch, target_level)
        };

        let Payload::Child(child) = &self.branches[branch_index].payload else {
            unreachable!("an internal R-tree branch always owns a child")
        };
        self.branches[branch_index].rect = child.cover();

        other
            .map(|child| Branch {
                rect: child.cover(),
                payload: Payload::Child(Box::new(child)),
            })
            .and_then(|branch| self.add(branch))
    }

    fn remove(&mut self, rect: Rect, item: usize, reinserts: &mut Vec<Self>) -> bool {
        if self.level == 0 {
            if let Some(index) = self.branches.iter().position(
                |branch| matches!(branch.payload, Payload::Item(candidate) if candidate == item),
            ) {
                self.branches.swap_remove(index);
                return true;
            }
            return false;
        }

        for index in 0..self.branches.len() {
            if !rect.overlaps(self.branches[index].rect) {
                continue;
            }
            let removed = {
                let Payload::Child(child) = &mut self.branches[index].payload else {
                    unreachable!("an internal R-tree branch always owns a child")
                };
                child.remove(rect, item, reinserts)
            };
            if !removed {
                continue;
            }
            let child_count = match &self.branches[index].payload {
                Payload::Child(child) => child.branches.len(),
                Payload::Item(_) => unreachable!(),
            };
            if child_count >= MIN_BRANCHES {
                let Payload::Child(child) = &self.branches[index].payload else {
                    unreachable!()
                };
                self.branches[index].rect = child.cover();
            } else {
                let removed = self.branches.swap_remove(index);
                let Payload::Child(child) = removed.payload else {
                    unreachable!()
                };
                reinserts.push(*child);
            }
            return true;
        }
        false
    }

    fn pick_branch(&self, rect: Rect) -> usize {
        let mut best = 0;
        let mut best_increase = f64::INFINITY;
        let mut best_area = f64::INFINITY;
        for (index, branch) in self.branches.iter().enumerate() {
            let area = branch.rect.spherical_volume();
            let increase = branch.rect.combine(rect).spherical_volume() - area;
            if increase < best_increase || (increase == best_increase && area < best_area) {
                best = index;
                best_increase = increase;
                best_area = area;
            }
        }
        best
    }

    fn split(&mut self, extra: Branch) -> Self {
        let mut branches = std::mem::take(&mut self.branches);
        branches.push(extra);
        let total = branches.len();
        let all_cover = branches
            .iter()
            .map(|branch| branch.rect)
            .reduce(Rect::combine)
            .unwrap();
        let areas = branches
            .iter()
            .map(|branch| branch.rect.spherical_volume())
            .collect::<Vec<_>>();

        let mut worst = -all_cover.spherical_volume() - 1.0;
        let mut seeds = (0, 0);
        for left in 0..total - 1 {
            for right in left + 1..total {
                let waste = branches[left]
                    .rect
                    .combine(branches[right].rect)
                    .spherical_volume()
                    - areas[left]
                    - areas[right];
                if waste >= worst {
                    worst = waste;
                    seeds = (left, right);
                }
            }
        }

        let mut partition = vec![None; total];
        let mut counts = [0_usize; 2];
        let mut covers = [None, None];
        let mut group_areas = [0.0; 2];
        classify(
            seeds.0,
            0,
            &branches,
            &mut partition,
            &mut counts,
            &mut covers,
            &mut group_areas,
        );
        classify(
            seeds.1,
            1,
            &branches,
            &mut partition,
            &mut counts,
            &mut covers,
            &mut group_areas,
        );

        while counts[0] + counts[1] < total
            && counts[0] < total - MIN_BRANCHES
            && counts[1] < total - MIN_BRANCHES
        {
            let mut biggest_difference = -1.0;
            let mut chosen = 0;
            let mut better_group = 0;
            for index in 0..total {
                if partition[index].is_some() {
                    continue;
                }
                let growth0 = covers[0]
                    .unwrap()
                    .combine(branches[index].rect)
                    .spherical_volume()
                    - group_areas[0];
                let growth1 = covers[1]
                    .unwrap()
                    .combine(branches[index].rect)
                    .spherical_volume()
                    - group_areas[1];
                let (difference, group) = if growth1 >= growth0 {
                    (growth1 - growth0, 0)
                } else {
                    (growth0 - growth1, 1)
                };
                if difference > biggest_difference
                    || (difference == biggest_difference && counts[group] < counts[better_group])
                {
                    biggest_difference = difference;
                    chosen = index;
                    better_group = group;
                }
            }
            classify(
                chosen,
                better_group,
                &branches,
                &mut partition,
                &mut counts,
                &mut covers,
                &mut group_areas,
            );
        }

        if counts[0] + counts[1] < total {
            let group = usize::from(counts[0] >= total - MIN_BRANCHES);
            for index in 0..total {
                if partition[index].is_none() {
                    classify(
                        index,
                        group,
                        &branches,
                        &mut partition,
                        &mut counts,
                        &mut covers,
                        &mut group_areas,
                    );
                }
            }
        }

        let mut other = Self {
            level: self.level,
            branches: Vec::with_capacity(MAX_BRANCHES),
        };
        for (branch, group) in branches.into_iter().zip(partition) {
            if group == Some(0) {
                self.branches.push(branch);
            } else {
                other.branches.push(branch);
            }
        }
        other
    }

    fn traverse(&self, output: &mut Vec<usize>) {
        for branch in &self.branches {
            match &branch.payload {
                Payload::Item(item) => output.push(*item),
                Payload::Child(child) => child.traverse(output),
            }
        }
    }
}

fn classify(
    index: usize,
    group: usize,
    branches: &[Branch],
    partition: &mut [Option<usize>],
    counts: &mut [usize; 2],
    covers: &mut [Option<Rect>; 2],
    areas: &mut [f64; 2],
) {
    partition[index] = Some(group);
    covers[group] = Some(match covers[group] {
        Some(cover) => cover.combine(branches[index].rect),
        None => branches[index].rect,
    });
    areas[group] = covers[group].unwrap().spherical_volume();
    counts[group] += 1;
}

#[cfg(test)]
pub(crate) fn traversal_order(entries: impl IntoIterator<Item = (i32, Bounds)>) -> Vec<usize> {
    traversal_order_with_refresh(
        entries
            .into_iter()
            .map(|(item_type, bounds)| (item_type, bounds, bounds)),
        0,
    )
}

pub(crate) fn traversal_order_with_refresh(
    entries: impl IntoIterator<Item = (i32, Bounds, Bounds)>,
    symbol_refresh_passes: usize,
) -> Vec<usize> {
    let entries = entries
        .into_iter()
        .map(|(item_type, initial, resolved)| {
            (
                item_type,
                Rect::from((item_type, initial)),
                Rect::from((item_type, resolved)),
            )
        })
        .collect::<Vec<_>>();
    let mut current = entries.iter().map(|entry| entry.1).collect::<Vec<_>>();
    let mut root = Node::leaf();
    for (item, (_, rect, _)) in entries.iter().copied().enumerate() {
        insert_root(
            &mut root,
            Branch {
                rect,
                payload: Payload::Item(item),
            },
            0,
        );
    }
    for _ in 0..symbol_refresh_passes {
        let mut order = Vec::new();
        root.traverse(&mut order);
        for item in order.into_iter().filter(|item| entries[*item].0 == 70) {
            let rect = current[item];
            let mut reinserts = Vec::new();
            assert!(root.remove(rect, item, &mut reinserts));
            while let Some(mut node) = reinserts.pop() {
                let target_level = node.level;
                for branch in node.branches.drain(..) {
                    insert_root(&mut root, branch, target_level);
                }
            }
            if root.level > 0 && root.branches.len() == 1 {
                let branch = root.branches.pop().unwrap();
                let Payload::Child(child) = branch.payload else {
                    unreachable!()
                };
                root = *child;
            }
            insert_root(
                &mut root,
                Branch {
                    rect: entries[item].2,
                    payload: Payload::Item(item),
                },
                0,
            );
            current[item] = entries[item].2;
        }
    }
    let mut order = Vec::new();
    if !root.branches.is_empty() {
        root.traverse(&mut order);
    }
    order
}

fn insert_root(root: &mut Node, branch: Branch, target_level: usize) {
    if let Some(other) = root.insert_branch(branch, target_level) {
        let old_root = std::mem::replace(root, Node::leaf());
        *root = Node {
            level: old_root.level + 1,
            branches: vec![
                Branch {
                    rect: old_root.cover(),
                    payload: Payload::Child(Box::new(old_root)),
                },
                Branch {
                    rect: other.cover(),
                    payload: Payload::Child(Box::new(other)),
                },
            ],
        };
    }
}

/// Reproduce libstdc++'s `std::sort` permutation, including the observable
/// ordering of comparator-equivalent values used by KiCad after traversal.
pub(crate) fn glibcxx_sort_by<T: Copy>(values: &mut [T], less: impl Fn(T, T) -> bool + Copy) {
    if values.len() < 2 {
        return;
    }
    let depth = 2 * (usize::BITS - values.len().leading_zeros() - 1) as usize;
    introsort_loop(values, 0, values.len(), depth, less);
    final_insertion_sort(values, less);
}

fn introsort_loop<T: Copy>(
    values: &mut [T],
    first: usize,
    mut last: usize,
    mut depth: usize,
    less: impl Fn(T, T) -> bool + Copy,
) {
    while last - first > 16 {
        if depth == 0 {
            values[first..last].sort_unstable_by(|left, right| {
                if less(*left, *right) {
                    std::cmp::Ordering::Less
                } else if less(*right, *left) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });
            return;
        }
        depth -= 1;
        let cut = partition_pivot(values, first, last, less);
        introsort_loop(values, cut, last, depth, less);
        last = cut;
    }
}

fn partition_pivot<T: Copy>(
    values: &mut [T],
    first: usize,
    last: usize,
    less: impl Fn(T, T) -> bool + Copy,
) -> usize {
    let middle = first + (last - first) / 2;
    move_median_to_first(values, first, first + 1, middle, last - 1, less);
    let pivot = values[first];
    let mut left = first + 1;
    let mut right = last;
    loop {
        while less(values[left], pivot) {
            left += 1;
        }
        right -= 1;
        while less(pivot, values[right]) {
            right -= 1;
        }
        if left >= right {
            return left;
        }
        values.swap(left, right);
        left += 1;
    }
}

fn move_median_to_first<T: Copy>(
    values: &mut [T],
    result: usize,
    left: usize,
    middle: usize,
    right: usize,
    less: impl Fn(T, T) -> bool + Copy,
) {
    let selected = if less(values[left], values[middle]) {
        if less(values[middle], values[right]) {
            middle
        } else if less(values[left], values[right]) {
            right
        } else {
            left
        }
    } else if less(values[left], values[right]) {
        left
    } else if less(values[middle], values[right]) {
        right
    } else {
        middle
    };
    values.swap(result, selected);
}

fn final_insertion_sort<T: Copy>(values: &mut [T], less: impl Fn(T, T) -> bool + Copy) {
    let guarded_end = values.len().min(16);
    insertion_sort(values, guarded_end, less);
    for index in guarded_end..values.len() {
        linear_insert(values, index, less);
    }
}

fn insertion_sort<T: Copy>(values: &mut [T], last: usize, less: impl Fn(T, T) -> bool + Copy) {
    for index in 1..last {
        let value = values[index];
        if less(value, values[0]) {
            values.copy_within(0..index, 1);
            values[0] = value;
        } else {
            linear_insert(values, index, less);
        }
    }
}

fn linear_insert<T: Copy>(values: &mut [T], index: usize, less: impl Fn(T, T) -> bool + Copy) {
    let value = values[index];
    let mut position = index;
    while position > 0 && less(value, values[position - 1]) {
        values[position] = values[position - 1];
        position -= 1;
    }
    values[position] = value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_survives_multiple_node_splits_without_loss() {
        let bounds = (0..80)
            .map(|index| Bounds {
                min_x: f64::from((index * 17) % 31),
                min_y: f64::from((index * 11) % 37),
                max_x: f64::from((index * 17) % 31) + 1.0,
                max_y: f64::from((index * 11) % 37) + 1.0,
            })
            .collect::<Vec<_>>();
        let first = traversal_order(bounds.iter().copied().map(|bounds| (0, bounds)));
        let second = traversal_order(bounds.iter().copied().map(|bounds| (0, bounds)));
        let mut sorted = first.clone();
        sorted.sort_unstable();

        assert_eq!(sorted, (0..bounds.len()).collect::<Vec<_>>());
        assert_eq!(first, second);
    }

    #[test]
    fn glibcxx_sort_keeps_keys_sorted_and_items_intact() {
        let mut values = (0..96).collect::<Vec<_>>();
        glibcxx_sort_by(&mut values, |left, right| left % 7 > right % 7);
        assert!(values.windows(2).all(|pair| pair[0] % 7 >= pair[1] % 7));
        let mut intact = values;
        intact.sort_unstable();
        assert_eq!(intact, (0..96).collect::<Vec<_>>());
    }
}
