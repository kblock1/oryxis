viewport: 1200x750
mode: Zen
-----
# Issue #208 item 4, first gesture: rearranging a tab's grid by dragging
# a pane. Needs the per-pane header, because the grid refuses to start a
# drag on a pane with no title bar (`Content::can_be_dragged_at`), so the
# prologue here is `pane-header.ice`'s: turn the preference on through
# the settings search, then build a split of two local shells.
#
# The press must land in the header's PICK AREA, which is the gap
# BETWEEN the label and the controls; the label text and the two buttons
# are deliberately not grabbable. With a shrink-width label on the left
# and shrink-width controls on the right, that gap is the middle of the
# strip: x=300 is inside it, y=54 is inside the 24 px header band.
#
# The move has to clear iced's own 10 px drag deadband (stricter than
# the tab strip's 6 px), hence the two `move` lines rather than one.
#
# What is asserted in TEXT is that the rearrange preserves: one pane is
# killed first, and its header still says so afterwards, which fails if
# the drop rebuilt or reset the pane instead of moving it. The resulting
# GEOMETRY (side by side becoming stacked) rides a screenshot, because
# the batch runner reports no coordinates for `find`, so there is no way
# to assert a pane's position in text.
#
# Unlike tab-drag-split.ice, the DROP here needs no screenshot to work.
# That one takes one because its drop hit-tests the app's own
# `Pane.bounds` cells, which only fill in when something draws; a drag
# that starts on a pane is hit-tested by the grid against its own layout
# tree, which the emulator has whether or not a frame was rendered.
#
# The two mid-gesture shots are about the PREVIEW rather than the drop.
# The highlight is the rectangle the pane lands in, which is not the
# region the cursor is over: dropping on a pane's edge closes the dragged
# pane first and the target grows into the gap, and a grid edge re-splits
# the root rather than the band being hovered. On this 1200 wide grid
# both shots should therefore show a HALF, x=600 w=600 over the right
# pane's right third and x=0 w=600 on the left rim, not the ~300 wide
# third and the ~27 wide band the cursor is actually inside.
expect "Welcome to Oryxis"
click "Skip"
click "Continue without password"
expect "Create host"
click (1168, 55)
expect "Terminal Settings"
click (95, 68)
type "title bar"
settle 400
expect "Title bar on each pane"
type enter
settle 500
click (1140, 70)
settle 400
click (333, 20)
expect "Local Shell"
timeout 800
click "Local Shell"
settle 900
type ctrl+shift+d
settle 800
click "Local Shell"
settle 1200
# Kill the focused (right) pane, so one of the two carries a verdict the
# drag must not disturb.
type "exit"
type enter
settle 1400
expect "bash (default) (disconnected)"
screenshot pane-drag-before
# Drag the LIVE pane by its header, onto the grid's bottom edge: the
# side-by-side split becomes a stacked one.
press (300, 54)
settle 200
move (340, 70)
settle 200
# Over the right pane's right third: the preview is the right half of
# the GRID, because the target inherits this pane's space on the way out.
move (1100, 400)
settle 400
screenshot pane-drag-preview-pane
# Over the grid's own left rim: the preview is the left half of the grid,
# not the thin band the cursor has to be inside to aim at it.
move (10, 400)
settle 400
screenshot pane-drag-preview-rim
move (900, 700)
settle 400
release (900, 700)
settle 900
# Both panes came through the move intact: the dead one still reports
# the end of its session, and the live one is still here.
expect "bash (default) (disconnected)"
expect "bash (default)"
screenshot pane-drag-stacked
