use blitz_traits::node_id::NodeId;
use std::{ops::Range, sync::Arc};
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;
use stylo_taffy::CopyStyleGetters;

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::properties::style_structs::Border;
use style::servo_arc::Arc as ServoArc;
use style::typed_om::NumericBaseType;
use style::values::computed::calc::CalcPercentageLeaf;
use style::values::computed::length_percentage::{
    CalcLengthPercentage, CalcNode, ComputedLeaf, Unpacked as UnpackedLengthPercentage,
};
use style::values::computed::{Length, LengthPercentage};
use style::values::generics::Optional;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout, values::computed::BorderStyle,
};
use style_traits::values::specified::AllowedNumericType;
use taffy::{
    CalcHandle, DetailedGridInfo, LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction,
    compute_block_layout, style_helpers,
};

use thin_vec::ThinVec;

use crate::layout::LayoutPassState;
use crate::node::NodeData;
use crate::{BaseDocument, taffy_node_id};

use super::construct::create_anonymous_block_node;
use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::resolve_calc_value;

// Deliberately not `Clone`: `style` may hold raw pointers into `calc_values`.
pub struct TableTreeWrapper<'a, 'doc> {
    pub(crate) doc: &'a mut LayoutPassState<'doc>,
    pub(crate) ctx: Arc<TableContext>,
}

/// A `display: table-caption` box placed above or below the table grid.
///
/// The caption is a sibling of the grid in the box tree, as in a browser's
/// table wrapper box, so the table element's own box is the wrapper that
/// contains both (<https://drafts.csswg.org/css-tables-3/#table-wrapper-box>).
#[derive(Debug, Clone)]
pub struct TableCaption {
    pub node_id: NodeId,
    /// The caption sits below the grid (`caption-side: bottom`).
    pub bottom: bool,
    /// The caption's box style, used for its block layout.
    pub style: taffy::Style<Atom>,
}

#[derive(Debug)]
pub struct TableContext {
    pub style: taffy::Style<Atom>,
    /// The wrapper box's style: the table's margins and positioning without the
    /// grid's border, padding or size, which belong to the grid.
    pub wrapper_style: taffy::Style<Atom>,
    /// Captions laid out above or below the grid, top captions first.
    pub captions: Vec<TableCaption>,
    /// The grid box's position and size inside the wrapper box, recorded by the
    /// wrapper's block layout (relative to the wrapper's border box).
    pub grid_layout: AtomicRefCell<Option<taffy::Layout>>,
    /// The wrapper box's size, which is the table element's box.
    pub wrapper_size: AtomicRefCell<Option<taffy::Size<f32>>>,
    pub cells: Vec<TableCell>,
    /// Absolutely positioned descendants of the table's row groups and rows (and of the
    /// table itself). They take no part in the table grid.
    pub oof_children: Vec<(NodeId, taffy::Position)>,
    pub rows: Vec<TableRow>,
    pub columns: Vec<TableColumn>,
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo<Atom>>>,
    pub border_style: Option<ServoArc<Border>>,
    pub border_collapse: BorderCollapse,
    /// Backing storage for `calc()` track sizes synthesised by the table layout code.
    /// Taffy stores calc values as raw pointers, so these must outlive the `style`.
    #[allow(dead_code)]
    calc_values: Vec<LengthPercentage>,
}

// #[derive(Debug, Clone, Eq, PartialEq)]
// pub enum TableItemKind {
//     Row,
//     Cell,
// }

#[derive(Debug, Clone)]
pub struct TableCell {
    // kind: TableItemKind,
    pub node_id: NodeId,
    /// Index (into `TableContext::rows`) of the row the cell originates in
    pub row: u16,
    /// Index of the column the cell originates in
    pub column: u16,
    style: taffy::Style<Atom>,
}

/// Tracks the current column position while walking the table's cells, so that
/// cells are assigned the same columns Taffy's dense auto-placement will give them
/// (skipping columns still occupied by rowspan cells from earlier rows).
#[derive(Debug, Default)]
struct ColumnCursor {
    /// The column the next cell in the current row will be placed in
    col: u16,
    /// The total number of columns seen so far
    num_columns: u16,
    /// For each column, the number of further rows it is occupied by a rowspan cell
    rowspans: Vec<u16>,
}

impl ColumnCursor {
    fn start_row(&mut self) {
        self.col = 0;
        for remaining in self.rowspans.iter_mut() {
            *remaining = remaining.saturating_sub(1);
        }
    }

    /// Advance past occupied columns, returning the column of the next cell
    fn next_free(&mut self) -> u16 {
        while self.rowspans.get(self.col as usize).is_some_and(|r| *r > 0) {
            self.col += 1;
        }
        self.col
    }

    fn place(&mut self, colspan: u16, rowspan: u16) {
        let end = self.col + colspan;
        if self.rowspans.len() < end as usize {
            self.rowspans.resize(end as usize, 0);
        }
        for remaining in &mut self.rowspans[self.col as usize..end as usize] {
            *remaining = rowspan - 1;
        }
        self.col = end;
        self.num_columns = self.num_columns.max(end);
    }
}

#[derive(Debug, Clone)]
pub struct TableColumn {
    pub node_id: NodeId,
    /// The column group (if any) that contains the column
    pub group: Option<NodeId>,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    // kind: TableItemKind,
    pub node_id: Option<NodeId>,
    /// The row group (if any) that contains the row
    pub group: Option<NodeId>,
    /// The row's specified `height`, if it is a length
    pub height: Option<f32>,
    /// Whether the row or any cell originating in it (and spanning only this row) has a
    /// specified length `height`. Such rows do not receive excess table height when there
    /// are unconstrained rows to receive it instead.
    pub is_constrained: bool,
}

/// The used width of one border side: border widths are not adjusted for
/// border-style in computed styles, so a border with `none`/`hidden` style
/// must be treated as zero-width.
/// The writing mode the table's grid runs in (the table's own)
#[cfg_attr(not(feature = "writing-mode"), allow(unused_variables))]
fn table_wm(doc: &BaseDocument, table_root_node_id: NodeId) -> WritingMode {
    #[cfg(feature = "writing-mode")]
    {
        if doc.try_root_element().map(|root| root.id) == Some(table_root_node_id) {
            doc.root_layout_wm(table_root_node_id)
        } else {
            doc.nodes[table_root_node_id].writing_mode()
        }
    }
    #[cfg(not(feature = "writing-mode"))]
    {
        WritingMode::empty()
    }
}

/// Convert a table part's style into the table's writing mode
fn table_taffy_style(style: &ComputedValues, table_wm: WritingMode) -> taffy::Style<Atom> {
    stylo_taffy::to_taffy_style_in(style, table_wm)
}

fn side_width(width: app_units::Au, style: BorderStyle) -> f32 {
    if style.none_or_hidden() {
        0.0
    } else {
        width.to_f32_px()
    }
}

/// Build a `calc(<percent> + <length>)` track sizing function. The calc value is
/// boxed and stored in `calc_values` so that the raw pointer Taffy holds stays valid.
fn percent_plus_length(
    calc_values: &mut Vec<LengthPercentage>,
    percent: f32,
    length: f32,
) -> TrackSizingFunction {
    let node = CalcNode::Sum(
        vec![
            CalcNode::Leaf(ComputedLeaf::Percentage(CalcPercentageLeaf::new(
                percent,
                Optional::Some(NumericBaseType::Length),
            ))),
            CalcNode::Leaf(ComputedLeaf::Length(Length::new(length))),
        ]
        .into(),
    );
    let value = LengthPercentage::new_calc(node, AllowedNumericType::NonNegative);
    let dim = match value.unpack() {
        UnpackedLengthPercentage::Calc(calc) => {
            let ptr = calc as *const CalcLengthPercentage as *const ();
            // SAFETY: the pointer targets the heap allocation owned by `value`, which
            // is kept alive by `calc_values` for as long as the TableContext exists.
            unsafe {
                taffy::Dimension::from_raw(taffy::CompactLength::calc(CalcHandle::from_ptr(ptr)))
            }
        }
        UnpackedLengthPercentage::Length(len) => style_helpers::length(len.px()),
        UnpackedLengthPercentage::Percentage(p) => style_helpers::percent(p.0),
    };
    calc_values.push(value);
    dim.into()
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: NodeId,
    anonymous_blocks: &mut ThinVec<NodeId>,
) -> (TableContext, Vec<NodeId>) {
    let mut cells: Vec<TableCell> = Vec::new();
    let mut oof_children: Vec<(NodeId, taffy::Position)> = Vec::new();
    let mut rows: Vec<TableRow> = Vec::new();
    let mut row = 0u16;
    let mut cursor = ColumnCursor::default();

    let wm = table_wm(doc, table_root_node_id);
    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = table_taffy_style(&stylo_styles, wm);
    style.item_is_table = true;
    // Use `dense` row-flow so that each cell scans the row from its
    // leftmost column for the first free track. Without `dense`,
    // `place_definite_secondary_axis_item` keeps a per-item secondary
    // cursor across rows, which means cells in later rows do not
    // backfill columns freed up by rowspan cells from earlier rows.
    style.grid_auto_flow = taffy::GridAutoFlow::RowDense;
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    // The fixed table layout algorithm only applies when the table has a non-auto width
    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => !style.size.width.is_auto(),
        TableLayout::Auto => false,
    };

    let border_collapse = stylo_styles.clone_border_collapse();
    let border_spacing = stylo_styles.clone_border_spacing().0;

    drop(stylo_styles);

    // A caption is not part of the grid: it is laid out by the table wrapper
    // box, above or below the grid, and widens the table element
    // (<https://drafts.csswg.org/css-tables-3/#caption>).
    let mut captions: Vec<TableCaption> = Vec::new();
    for child_id in children.iter().copied() {
        let node = &doc.nodes[child_id];
        if !node
            .display_style()
            .is_some_and(|display| display.outside() == DisplayOutside::TableCaption)
        {
            continue;
        }
        let Some(styles) = node.primary_styles() else {
            continue;
        };
        captions.push(TableCaption {
            node_id: child_id,
            bottom: styles.slow_clone_caption_side()
                == style::values::computed::table::CaptionSide::Bottom,
            style: table_taffy_style(&styles, wm),
        });
    }
    // Top captions stack above the grid, bottom captions below it, each side in
    // document order.
    captions.sort_by_key(|caption| caption.bottom);

    let mut columns: Vec<TableColumn> = Vec::new();
    let mut column_sizes: Vec<taffy::TrackSizingFunction> = Vec::new();
    for child_id in children.iter().copied() {
        collect_columns(doc, child_id, wm, &mut columns, &mut column_sizes);
    }
    // Percentage column widths only take effect in the fixed table layout algorithm
    if !is_fixed {
        for column in column_sizes.iter_mut() {
            if column.max.into_raw().tag() == taffy::CompactLength::PERCENT_TAG {
                *column = style_helpers::auto();
            }
        }
    }
    let mut first_cell_border: Option<ServoArc<Border>> = None;
    // Percentage widths set on first-row cells: (column index, percentage, padding + border)
    let mut percent_columns: Vec<(u16, f32, f32)> = Vec::new();
    let mut calc_values: Vec<LengthPercentage> = Vec::new();
    // Header row groups are laid out before other rows, and footer row groups after
    let row_group_order = |doc: &BaseDocument, child_id: NodeId| -> u8 {
        let display = doc.nodes[child_id]
            .primary_styles()
            .map(|s| s.clone_display());
        match display.map(|d| d.inside()) {
            Some(DisplayInside::TableHeaderGroup) => 0,
            Some(DisplayInside::TableFooterGroup) => 2,
            _ => 1,
        }
    };
    {
        let mut collector = TableCollector {
            doc,
            table_root_node_id,
            wm,
            is_fixed,
            border_collapse,
            row: &mut row,
            cursor: &mut cursor,
            cells: &mut cells,
            oof_children: &mut oof_children,
            rows: &mut rows,
            columns: &mut column_sizes,
            first_cell_border: &mut first_cell_border,
            percent_columns: &mut percent_columns,
            anonymous_blocks,
        };
        for order in 0..3 {
            let mut table_run = AnonymousRun::new(false);
            for child_id in children.iter().copied() {
                if captions.iter().any(|caption| caption.node_id == child_id) {
                    continue;
                }
                if row_group_order(collector.doc, child_id) != order {
                    continue;
                }
                collector.collect_child(table_root_node_id, child_id, &mut table_run);
            }
        }
    }
    let remaining_column = if is_fixed {
        style_helpers::minmax(style_helpers::length(0.0), style_helpers::fr(1.0))
    } else {
        style_helpers::auto()
    };
    // Trailing `<col>` columns which contain no cells and have no definite width
    // (i.e. would be zero-width) are dropped, so they don't add border-spacing.
    while column_sizes.len() > cursor.num_columns as usize
        && column_sizes
            .last()
            .is_some_and(|c| c.min.into_raw().tag() == taffy::CompactLength::AUTO_TAG)
    {
        column_sizes.pop();
        columns.pop();
    }
    let num_columns = cursor.num_columns.max(column_sizes.len() as u16);
    column_sizes.resize(num_columns as usize, remaining_column);
    if is_fixed {
        // In the fixed table layout algorithm, percentage column widths resolve against
        // the table's content width minus the horizontal border-spacing between columns,
        // whereas Taffy resolves percentage tracks against the container's inner width
        // (from which only the outer border-spacing has been removed via padding). Fold
        // the inner border-spacing into the percentage using `calc(N% - N * spacing)`.
        let spacing_x = match border_collapse {
            BorderCollapse::Separate => border_spacing.width.px(),
            BorderCollapse::Collapse => 0.0,
        };
        let inner_spacing = spacing_x * num_columns.saturating_sub(1) as f32;
        let mut percent_track = |percent: f32, extra: f32| -> TrackSizingFunction {
            let extra = extra - percent * inner_spacing;
            if extra == 0.0 {
                style_helpers::percent(percent)
            } else {
                percent_plus_length(&mut calc_values, percent, extra)
            }
        };

        for column in column_sizes.iter_mut() {
            if column.max.is_auto() {
                *column = remaining_column;
            } else if column.max.into_raw().tag() == taffy::CompactLength::PERCENT_TAG {
                *column = percent_track(column.max.into_raw().value(), 0.0);
            }
        }

        // Percentage widths on cells apply to the cell's content box, so the
        // cell's horizontal padding and border must be added to the column width.
        for (col_idx, percent, extra) in percent_columns {
            if let Some(column) = column_sizes.get_mut(col_idx as usize) {
                *column = percent_track(percent, extra);
            }
        }
    }

    style.grid_template_columns = column_sizes.into_iter().map(|dim| dim.into()).collect();
    // A row's `height` is a minimum: the row grows to fit the content of its cells.
    // Excess table height is distributed to unconstrained rows (via the stretching of
    // `auto` tracks) if there are any, and to all rows otherwise. So constrained rows
    // use a `max-content` (rather than `auto`) maximum when there are unconstrained rows.
    let has_unconstrained_rows = rows.iter().any(|row| !row.is_constrained);
    style.grid_template_rows = rows
        .iter()
        .map(|row| {
            let min = match row.height {
                Some(height) => style_helpers::length(height),
                None => style_helpers::auto(),
            };
            let max = if row.is_constrained && has_unconstrained_rows {
                style_helpers::max_content()
            } else {
                style_helpers::auto()
            };
            style_helpers::minmax(min, max)
        })
        .collect();

    style.gap = match border_collapse {
        BorderCollapse::Separate => {
            // In the separated borders model, `border-spacing` also applies between
            // the table border and the outermost cells, in addition to between cells.
            // Border-spacing surrounds the grid. A table with neither a cell nor
            // an explicit row or column has an empty grid, which has no spacing;
            // explicit tracks still separate (and so add spacing around) the
            // anonymous cells the grid generates for them
            // (<https://drafts.csswg.org/css-tables-3/#missing-cells-fixup>).
            let spaced = !cells.is_empty() || !rows.is_empty() || !columns.is_empty();
            let spacing_x = if spaced {
                border_spacing.width.px()
            } else {
                0.0
            };
            let spacing_y = if spaced {
                border_spacing.height.px()
            } else {
                0.0
            };
            let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
            style.padding = taffy::Rect {
                left: style_helpers::length(padding.left + spacing_x),
                right: style_helpers::length(padding.right + spacing_x),
                top: style_helpers::length(padding.top + spacing_y),
                bottom: style_helpers::length(padding.bottom + spacing_y),
            };
            taffy::Size {
                width: style_helpers::length(spacing_x),
                height: style_helpers::length(spacing_y),
            }
        }
        BorderCollapse::Collapse => first_cell_border
            .as_ref()
            .map(|border| {
                let x = side_width(border.border_left_width.0, border.border_left_style).max(
                    side_width(border.border_right_width.0, border.border_right_style),
                );
                let y = side_width(border.border_top_width.0, border.border_top_style).max(
                    side_width(border.border_bottom_width.0, border.border_bottom_style),
                );
                taffy::Size {
                    width: style_helpers::length(x),
                    height: style_helpers::length(y),
                }
            })
            .unwrap_or(taffy::Size::ZERO.map(style_helpers::length)),
    };

    if border_collapse == BorderCollapse::Collapse {
        style.border = taffy::Rect {
            left: style.gap.width,
            right: style.gap.width,
            top: style.gap.height,
            bottom: style.gap.height,
        };
    }

    // The caption boxes are children of the element in the box tree: they paint
    // and hit test as part of it, and their own content is constructed from the
    // layout children.
    let layout_children = cells
        .iter()
        .map(|cell| cell.node_id)
        .chain(oof_children.iter().map(|(node_id, _)| *node_id))
        .chain(captions.iter().map(|caption| caption.node_id))
        .collect();
    // With captions the element's box is the wrapper: an anonymous block that
    // holds the captions and the grid. The wrapper takes the table's margin,
    // positioning and insets; the grid keeps the border, padding and size, and
    // is laid out in flow inside the wrapper
    // (<https://drafts.csswg.org/css-tables-3/#table-wrapper-box>).
    let mut wrapper_style = taffy::Style::<Atom>::DEFAULT;
    wrapper_style.display = taffy::Display::Block;
    wrapper_style.position = style.position;
    wrapper_style.inset = style.inset;
    wrapper_style.margin = style.margin;
    wrapper_style.box_sizing = taffy::BoxSizing::BorderBox;
    if !captions.is_empty() {
        style.position = taffy::Position::Static;
        style.inset = taffy::Rect::auto();
        style.margin = taffy::Rect::ZERO.map(style_helpers::length);
        // `float` and `clear` also belong to the wrapper
        // (<https://www.w3.org/TR/CSS21/tables.html#table-box>): the grid is
        // always in flow inside it, so leaving them on the grid would float the
        // grid inside its own wrapper.
        #[cfg(feature = "floats")]
        {
            wrapper_style.float = style.float;
            wrapper_style.clear = style.clear;
            style.float = taffy::Float::None;
            style.clear = taffy::Clear::None;
        }
    }

    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            wrapper_style,
            captions,
            grid_layout: AtomicRefCell::new(None),
            wrapper_size: AtomicRefCell::new(None),
            cells,
            oof_children,
            rows,
            columns,
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            border_style: first_cell_border,
            calc_values,
        },
        layout_children,
    )
}

/// Collect `<col>` elements (and `display: table-column` elements) along with their
/// widths, which take precedence over cell widths in the fixed table layout algorithm.
/// Columns without a specified width are recorded as `auto`.
fn collect_columns(
    doc: &mut BaseDocument,
    node_id: NodeId,
    wm: WritingMode,
    columns: &mut Vec<TableColumn>,
    column_sizes: &mut Vec<TrackSizingFunction>,
) {
    let node = &doc.nodes[node_id];
    if !node.is_element() {
        return;
    }
    let Some(display) = node.display_style() else {
        return;
    };

    match display.inside() {
        DisplayInside::TableColumnGroup => {
            let first_column = columns.len();
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_columns(doc, child_id, wm, columns, column_sizes);
            }
            doc.nodes[node_id].children = children;
            for column in &mut columns[first_column..] {
                column.group.get_or_insert(node_id);
            }
        }
        DisplayInside::TableColumn => {
            let style = table_taffy_style(&node.primary_styles().unwrap(), wm);
            let span: u16 = node
                .attr(local_name!("span"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.max(1))
                .unwrap_or(1);
            let column: TrackSizingFunction = match style.size.width.tag() {
                taffy::CompactLength::LENGTH_TAG => {
                    // A definite `max-width` clamps the column width; a zero width is treated as auto
                    let mut width = style.size.width.value();
                    if style.max_size.width.into_raw().tag() == taffy::CompactLength::LENGTH_TAG {
                        width = width.min(style.max_size.width.into_raw().value());
                    }
                    if width > 0.0 {
                        style_helpers::length(width)
                    } else {
                        style_helpers::auto()
                    }
                }
                taffy::CompactLength::PERCENT_TAG => {
                    style_helpers::percent(style.size.width.value())
                }
                // Browsers treat calc() widths on columns as auto
                _ => style_helpers::auto(),
            };
            // A definite `min-width` on a column acts as a floor on its width
            let column = match style.min_size.width.into_raw().tag() {
                taffy::CompactLength::LENGTH_TAG if column.max.is_auto() => style_helpers::minmax(
                    style_helpers::length(style.min_size.width.into_raw().value()),
                    style_helpers::auto(),
                ),
                _ => column,
            };
            for _ in 0..span {
                columns.push(TableColumn {
                    node_id,
                    group: None,
                });
                column_sizes.push(column);
            }
        }
        _ => {}
    }
}

// https://drafts.csswg.org/css-tables-3/#fixup
struct AnonymousRun {
    in_explicit_row: bool,
    anon_row_open: bool,
    anon_cell: Option<NodeId>,
    pending_ws: Vec<NodeId>,
}

impl AnonymousRun {
    fn new(in_explicit_row: bool) -> Self {
        Self {
            in_explicit_row,
            anon_row_open: false,
            anon_cell: None,
            pending_ws: Vec::new(),
        }
    }

    fn flush_cell(&mut self) {
        self.anon_cell = None;
        self.pending_ws.clear();
    }

    fn flush_row(&mut self) {
        self.flush_cell();
        self.anon_row_open = false;
    }
}

struct TableCollector<'a> {
    doc: &'a mut BaseDocument,
    table_root_node_id: NodeId,
    wm: WritingMode,
    is_fixed: bool,
    border_collapse: BorderCollapse,
    row: &'a mut u16,
    cursor: &'a mut ColumnCursor,
    cells: &'a mut Vec<TableCell>,
    oof_children: &'a mut Vec<(NodeId, taffy::Position)>,
    rows: &'a mut Vec<TableRow>,
    columns: &'a mut Vec<TrackSizingFunction>,
    first_cell_border: &'a mut Option<ServoArc<Border>>,
    percent_columns: &'a mut Vec<(u16, f32, f32)>,
    anonymous_blocks: &'a mut ThinVec<NodeId>,
}

impl TableCollector<'_> {
    fn ensure_row(&mut self, run: &mut AnonymousRun) {
        if !run.in_explicit_row && !run.anon_row_open {
            *self.row += 1;
            self.cursor.start_row();
            self.rows.push(TableRow {
                node_id: None,
                group: None,
                height: None,
                is_constrained: false,
            });
            run.anon_row_open = true;
        }
    }

    fn push_improper_child(
        &mut self,
        style_parent_id: NodeId,
        child_id: NodeId,
        run: &mut AnonymousRun,
    ) {
        self.ensure_row(run);
        let anon_id = match run.anon_cell {
            Some(id) => id,
            None => {
                let anon_id =
                    create_anonymous_block_node(self.doc, self.table_root_node_id, style_parent_id);
                self.anonymous_blocks.push(anon_id);

                let col = self.cursor.next_free();
                let mut style =
                    table_taffy_style(&self.doc.nodes[anon_id].primary_styles().unwrap(), self.wm);
                if self.border_collapse == BorderCollapse::Collapse {
                    style.border = taffy::Rect::ZERO.map(style_helpers::length);
                }
                if self.is_fixed {
                    style.min_size.width = style_helpers::length(0.0);
                }
                if *self.row == 1 && (col as usize) >= self.columns.len() {
                    self.columns.resize(col as usize, style_helpers::auto());
                    self.columns.push(style_helpers::auto());
                }
                style.margin = taffy::Rect::ZERO.map(style_helpers::length);
                style.grid_column = taffy::Line {
                    start: style_helpers::auto(),
                    end: style_helpers::span(1),
                };
                style.grid_row = taffy::Line {
                    start: style_helpers::line(*self.row as i16),
                    end: style_helpers::span(1),
                };
                style.size.width = style_helpers::auto();
                self.cells.push(TableCell {
                    node_id: anon_id,
                    row: self.row.saturating_sub(1),
                    column: col,
                    style,
                });
                self.cursor.place(1, 1);
                run.anon_cell = Some(anon_id);
                anon_id
            }
        };
        let anon_children = &mut self.doc.nodes[anon_id].children;
        anon_children.extend(run.pending_ws.drain(..));
        anon_children.push(child_id);
    }

    fn collect_child(&mut self, style_parent_id: NodeId, node_id: NodeId, run: &mut AnonymousRun) {
        match &self.doc.nodes[node_id].data {
            NodeData::Comment { .. }
            | NodeData::Doctype { .. }
            | NodeData::ProcessingInstruction { .. }
            | NodeData::Document(_)
            | NodeData::Fragment { .. }
            | NodeData::ShadowRoot { .. } => return,
            NodeData::Text(text) => {
                if text.content.chars().all(char::is_whitespace) {
                    if run.anon_cell.is_some() {
                        run.pending_ws.push(node_id);
                    }
                } else {
                    self.push_improper_child(style_parent_id, node_id, run);
                }
                return;
            }
            NodeData::CDataSection { contents } => {
                if contents.chars().all(char::is_whitespace) {
                    if run.anon_cell.is_some() {
                        run.pending_ws.push(node_id);
                    }
                } else {
                    self.push_improper_child(style_parent_id, node_id, run);
                }
                return;
            }
            NodeData::Element(_) | NodeData::AnonymousBlock(_) => {}
        }

        let node = &mut self.doc.nodes[node_id];
        let Some(display) = node.display_style() else {
            #[cfg(feature = "tracing")]
            tracing::info!("Ignoring table descendent because it has no styles");
            return;
        };

        if display.outside() == DisplayOutside::None {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            return;
        }

        // Absolutely positioned boxes are out-of-flow: they are laid out by their containing
        // block rather than as part of the table grid.
        let position = node.primary_styles().unwrap().clone_position();
        if position.is_absolutely_positioned() {
            self.oof_children
                .push((node_id, stylo_taffy::convert::position(position)));
            return;
        }

        match display.inside() {
            DisplayInside::Contents => {
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                let children = std::mem::take(&mut self.doc.nodes[node_id].children);
                for child_id in children.iter().copied() {
                    self.collect_child(node_id, child_id, run);
                }
                self.doc.nodes[node_id].children = children;
            }
            DisplayInside::TableRowGroup
            | DisplayInside::TableHeaderGroup
            | DisplayInside::TableFooterGroup => {
                run.flush_row();
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                let first_row = self.rows.len();
                let children = std::mem::take(&mut self.doc.nodes[node_id].children);
                let mut group_run = AnonymousRun::new(false);
                for child_id in children.iter().copied() {
                    self.collect_child(node_id, child_id, &mut group_run);
                }
                self.doc.nodes[node_id].children = children;
                for row in &mut self.rows[first_row..] {
                    row.group.get_or_insert(node_id);
                }
            }
            DisplayInside::TableRow => {
                run.flush_row();
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                *self.row += 1;
                self.cursor.start_row();

                let height = node.primary_styles().and_then(|style| {
                    let height = stylo_taffy::convert::dimension(style.get_height());
                    (height.tag() == taffy::CompactLength::LENGTH_TAG).then(|| height.value())
                });
                self.rows.push(TableRow {
                    node_id: Some(node_id),
                    group: None,
                    height,
                    is_constrained: height.is_some(),
                });

                let children = std::mem::take(&mut self.doc.nodes[node_id].children);
                let mut row_run = AnonymousRun::new(true);
                for child_id in children.iter().copied() {
                    self.collect_child(node_id, child_id, &mut row_run);
                }
                self.doc.nodes[node_id].children = children;
            }
            DisplayInside::TableCell => {
                run.flush_cell();
                self.ensure_row(run);
                let node = &self.doc.nodes[node_id];
                let stylo_style = &node.primary_styles().unwrap();
                let colspan: u16 = node
                    .attr(local_name!("colspan"))
                    .and_then(|val| val.parse().ok())
                    .unwrap_or(1);
                let rowspan: u16 = node
                    .attr(local_name!("rowspan"))
                    .and_then(|val| val.parse::<u16>().ok())
                    .map(|v| v.clamp(1, 65534))
                    .unwrap_or(1);
                let mut style = table_taffy_style(stylo_style, self.wm);
                let col = self.cursor.next_free();

                // In the collapsed borders model the borders are laid out as gutters between
                // the cells (see the table's `gap`) rather than as part of the cells
                if self.border_collapse == BorderCollapse::Collapse {
                    style.border = taffy::Rect::ZERO.map(style_helpers::length);
                }

                if self.first_cell_border.is_none() {
                    *self.first_cell_border = Some(stylo_style.clone_border());
                }

                // In the fixed table layout algorithm the widths of columns are not
                // affected by cell contents, so cells must not impose a min-content
                // floor on their tracks.
                if self.is_fixed {
                    style.min_size.width = style_helpers::length(0.0);
                }

                // Column widths from `<col>` elements take precedence over cell widths
                let col_needs_width = (col as usize) >= self.columns.len()
                    || self.columns[col as usize].max.is_auto();
                if *self.row == 1 && col_needs_width {
                    let column: TrackSizingFunction = match style.size.width.tag() {
                        taffy::CompactLength::LENGTH_TAG => {
                            let len = style.size.width.value();
                            let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
                            let border = style.border.resolve_or_zero(None, resolve_calc_value);
                            match style.box_sizing {
                                taffy::BoxSizing::ContentBox => style_helpers::length(
                                    len + padding.left + padding.right + border.left + border.right,
                                ),
                                taffy::BoxSizing::BorderBox => style_helpers::length(len),
                            }
                        }
                        taffy::CompactLength::PERCENT_TAG => {
                            if self.is_fixed {
                                let extra = match style.box_sizing {
                                    taffy::BoxSizing::ContentBox => {
                                        let padding =
                                            style.padding.resolve_or_zero(None, resolve_calc_value);
                                        let border =
                                            style.border.resolve_or_zero(None, resolve_calc_value);
                                        padding.left + padding.right + border.left + border.right
                                    }
                                    taffy::BoxSizing::BorderBox => 0.0,
                                };
                                // Resolved in `build_table_context` once the column count is known
                                self.percent_columns
                                    .push((col, style.size.width.value(), extra));
                                style_helpers::percent(style.size.width.value())
                            } else {
                                style_helpers::auto()
                            }
                        }
                        taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
                        // Dimension values are always length, percentage, auto or calc(),
                        // so any other tag is a calc() value. Pass it through so that
                        // Taffy resolves it against the table's inner width.
                        _ => style.size.width.into(),
                    };
                    if (col as usize) < self.columns.len() {
                        if !column.max.is_auto() {
                            self.columns[col as usize] = column;
                        }
                    } else {
                        self.columns.resize(col as usize, style_helpers::auto());
                        self.columns.push(column);
                    }
                }

                // The margin properties do not apply to table-internal elements
                style.margin = taffy::Rect::ZERO.map(style_helpers::length);

                // Let Taffy auto-place the column. Combined with
                // `grid_auto_flow: RowDense` set on the table root, each cell
                // scans from the first track in its row for a free position,
                // which makes cells automatically skip columns occupied by
                // rowspan cells from earlier rows.
                style.grid_column = taffy::Line {
                    start: style_helpers::auto(),
                    end: style_helpers::span(colspan),
                };
                style.grid_row = taffy::Line {
                    start: style_helpers::line(*self.row as i16),
                    end: style_helpers::span(rowspan),
                };
                if rowspan == 1 && style.size.height.tag() == taffy::CompactLength::LENGTH_TAG {
                    if let Some(row) = self.rows.last_mut() {
                        row.is_constrained = true;
                    }
                }

                style.size.width = style_helpers::auto();
                self.cells.push(TableCell {
                    node_id,
                    row: self.row.saturating_sub(1),
                    column: col,
                    style,
                });

                self.cursor.place(colspan, rowspan);
            }
            DisplayInside::Flow
            | DisplayInside::FlowRoot
            | DisplayInside::Flex
            | DisplayInside::Grid
            | DisplayInside::GridLanes
            | DisplayInside::Table => {
                self.push_improper_child(style_parent_id, node_id, run);
            }
            DisplayInside::TableColumnGroup | DisplayInside::TableColumn => {
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                run.flush_cell();
            }
            DisplayInside::None => {
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
        }
    }
}

/// The table wrapper box: the anonymous block a browser puts around a table's
/// captions and grid. It exists only in layout; the element's own box is this
/// wrapper, and its children are the caption boxes and the grid
/// (<https://drafts.csswg.org/css-tables-3/#table-wrapper-box>).
///
/// The wrapper names its children by their real node ids rather than by wrapper
/// positions: taffy records out-of-flow candidates and unrounded layouts against
/// the ids it is handed, and Blitz reads those ids back as DOM nodes.
pub struct TableWrapperTree<'a, 'doc> {
    doc: &'a mut LayoutPassState<'doc>,
    ctx: Arc<TableContext>,
    /// The grid's node: the table element itself.
    grid: taffy::NodeId,
    /// The wrapper's children in flow order: the top captions, the grid, then
    /// the bottom captions.
    children: Vec<taffy::NodeId>,
}

impl<'a, 'doc> TableWrapperTree<'a, 'doc> {
    fn new(
        doc: &'a mut LayoutPassState<'doc>,
        ctx: Arc<TableContext>,
        grid: taffy::NodeId,
    ) -> Self {
        // `captions` is sorted with the bottom captions last, so the grid goes
        // before the first one.
        let mut children = Vec::with_capacity(ctx.captions.len() + 1);
        let mut placed = false;
        for caption in &ctx.captions {
            if caption.bottom && !placed {
                children.push(grid);
                placed = true;
            }
            children.push(taffy_node_id(caption.node_id));
        }
        if !placed {
            children.push(grid);
        }
        Self {
            doc,
            ctx,
            grid,
            children,
        }
    }

    fn caption(&self, node_id: taffy::NodeId) -> Option<&TableCaption> {
        self.ctx
            .captions
            .iter()
            .find(|caption| taffy_node_id(caption.node_id) == node_id)
    }
}

impl taffy::TraversePartialTree for TableWrapperTree<'_, '_> {
    type ChildIter<'a>
        = std::iter::Copied<std::slice::Iter<'a, taffy::NodeId>>
    where
        Self: 'a;

    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        self.children.iter().copied()
    }

    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.children.len()
    }

    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        self.children[index]
    }
}
impl taffy::TraverseTree for TableWrapperTree<'_, '_> {}

impl taffy::LayoutPartialTree for TableWrapperTree<'_, '_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.wrapper_style
    }

    fn resolve_calc_value(&self, calc: CalcHandle, parent_size: f32) -> f32 {
        resolve_calc_value(calc, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        // The grid's layout is where the table's own box sits inside the
        // wrapper. The element's box is re-expressed from it once the parent
        // has placed the wrapper
        // (<https://drafts.csswg.org/css-tables-3/#table-wrapper-box>).
        if node_id == self.grid {
            *self.ctx.grid_layout.borrow_mut() = Some(*layout);
            return;
        }
        self.doc.set_unrounded_layout(node_id, layout);
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::LayoutInput,
    ) -> taffy::LayoutOutput {
        if node_id == self.grid {
            let mut grid = TableTreeWrapper {
                doc: self.doc,
                ctx: self.ctx.clone(),
            };
            return taffy::compute_grid_layout(&mut grid, node_id, inputs);
        }
        if self.caption(node_id).is_some() {
            return self.doc.compute_child_layout(node_id, inputs);
        }
        taffy::LayoutOutput::HIDDEN
    }
}

impl taffy::LayoutBlockContainer for TableWrapperTree<'_, '_> {
    type BlockContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type BlockItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_block_container_style(&self, _node_id: taffy::NodeId) -> Self::BlockContainerStyle<'_> {
        &self.ctx.wrapper_style
    }

    fn get_block_child_style(&self, child_node_id: taffy::NodeId) -> Self::BlockItemStyle<'_> {
        if child_node_id == self.grid {
            &self.ctx.style
        } else {
            self.caption(child_node_id)
                .map_or(&self.ctx.style, |caption| &caption.style)
        }
    }
}

/// Lay out a table that has captions: the table element's box is the wrapper
/// box, so its size is the wider of the grid and the captions, and its height
/// sums the captions and the grid
/// (<https://drafts.csswg.org/css-tables-3/#table-wrapper-box>).
///
/// `grid` is the grid's layout from the table's own inputs, used to size the
/// wrapper; the grid is then laid out again inside the wrapper, where an `auto`
/// width stretches to the wrapper.
pub(crate) fn layout_table_with_captions(
    doc: &mut LayoutPassState<'_>,
    table_node_id: taffy::NodeId,
    inputs: taffy::LayoutInput,
    context: Arc<TableContext>,
    grid: &taffy::LayoutOutput,
) -> taffy::LayoutOutput {
    // The wrapper is as wide as the widest of the grid and the captions'
    // margin boxes, but no wider than the containing block when that block is
    // wider than the table: a shrink-to-fit ancestor sized from the table
    // itself must not shrink a wider caption, since the table's preferred width
    // includes its captions
    // (<https://drafts.csswg.org/css-tables-3/#computing-the-table-width>).
    let mut width = grid.size.width;
    for caption in &context.captions {
        width = width.max(caption_margin_box(doc, caption, inputs.parent_size));
    }
    if let Some(available) = inputs.parent_size.width
        && available > grid.size.width
    {
        width = width.min(available);
    }

    let mut wrapper_inputs = inputs;
    wrapper_inputs.known_dimensions.width = Some(width);
    wrapper_inputs.known_dimensions.height = None;

    let mut tree = TableWrapperTree::new(doc, context.clone(), table_node_id);
    let output = compute_block_layout(&mut tree, table_node_id, wrapper_inputs, None);
    *context.wrapper_size.borrow_mut() = Some(output.size);

    // The wrapper positioned the captions in the wrapper's own coordinate
    // space, but a caption's parent is the table element, whose box is the grid
    // inside the wrapper: move the captions onto that origin so that paint, hit
    // testing, and offsets agree
    // (<https://drafts.csswg.org/css-tables-3/#table-wrapper-box>).
    let grid_offset = context
        .grid_layout
        .borrow()
        .map_or(taffy::Point::ZERO, |layout| layout.location);
    for caption in &context.captions {
        let layout = doc.nodes[caption.node_id].unrounded_layout_mut();
        layout.location.x -= grid_offset.x;
        layout.location.y -= grid_offset.y;
    }
    output
}

/// The width of a caption's margin box, which is the part that widens the table
/// wrapper.
fn caption_margin_box(
    doc: &mut LayoutPassState<'_>,
    caption: &TableCaption,
    parent_size: taffy::Size<Option<f32>>,
) -> f32 {
    let inputs = taffy::LayoutInput {
        run_mode: taffy::RunMode::ComputeSize,
        sizing_mode: taffy::SizingMode::InherentSize,
        axis: taffy::RequestedAxis::Horizontal,
        known_dimensions: taffy::Size::NONE,
        known_dimensions_are_definite: taffy::Size {
            width: false,
            height: false,
        },
        parent_size,
        available_space: taffy::Size {
            width: taffy::AvailableSpace::MinContent,
            height: taffy::AvailableSpace::MaxContent,
        },
        vertical_margins_are_collapsible: taffy::Line::FALSE,
    };
    let output = doc.compute_child_layout(taffy_node_id(caption.node_id), inputs);
    let margin = caption
        .style
        .margin
        .resolve_or_zero(parent_size.width, resolve_calc_value);
    output.size.width + margin.left + margin.right
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_, '_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.cells.len())
    }

    #[inline(always)]
    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.ctx.cells.len()
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_, '_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_, '_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.style
    }

    fn resolve_calc_value(&self, calc: CalcHandle, parent_size: f32) -> f32 {
        resolve_calc_value(calc, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let node_id = crate::taffy_node_id(self.ctx.cells[usize::from(node_id)].node_id);
        self.doc.set_unrounded_layout(node_id, layout)
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.cells[usize::from(node_id)];
        let node_id = crate::taffy_node_id(cell.node_id);
        self.doc.compute_child_layout(node_id, inputs)
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_, '_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.cells[usize::from(child_node_id)].style
    }

    fn set_detailed_grid_info(
        &mut self,
        _node_id: taffy::NodeId,
        detailed_grid_info: DetailedGridInfo<Atom>,
    ) {
        *self.ctx.computed_grid_info.borrow_mut() = Some(detailed_grid_info);
    }
}
