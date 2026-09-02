viewport: 1200x750
mode: Zen
-----
# Issue #208 item 2: the optional per-pane title bar. Off by default and
# split-only, so this has to turn it on, build a split, and kill one
# pane. Two local shells, which is the only split a headless run can
# build without a host.
#
# The setting is reached through the settings SEARCH rather than by
# scrolling: the row sits ~2000 px down its section, out of a 750 px
# viewport, and the search reveal scrolls it to a fixed place at the top.
# That is also what pins the one coordinate here. Clicking the LABEL does
# nothing (a `nav_toggle_row` puts the switch on the right), so the click
# has to land on the toggler. To re-measure after a layout change, take a
# screenshot right after the `type enter` below and read the switch off
# it; `find` reports content coordinates, not viewport ones, so it cannot
# answer this.
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
# A second tab, leaving Settings open so the `+` keeps its position.
click (333, 20)
expect "Local Shell"
timeout 800
click "Local Shell"
settle 900
type ctrl+shift+d
settle 800
click "Local Shell"
settle 1200
screenshot pane-header-split
# The focused pane's shell exits. Its header says so IN WORDS, not just
# by tinting a dot, and the card that answers for a headerless pane
# stands down because the header already offers the same two actions.
type "exit"
type enter
settle 1400
expect "bash (default) (disconnected)"
absent "Session ended"
screenshot pane-header-ended
