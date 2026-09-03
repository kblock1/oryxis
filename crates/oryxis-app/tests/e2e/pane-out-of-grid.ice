viewport: 1200x750
mode: Zen
-----
# Issue #208 item 4, the two gestures that take a pane OUT of its grid:
# dragged by its header onto another tab's chip, and broken out into a
# tab of its own by each of its four doors (tab menu, header menu, the
# chord, the header button). Same prologue as pane-drag.ice: headers on
# through the settings search, then a split of two local shells.
#
# The pane that travels is KILLED first, so every landing can be told
# apart from a rebuild: a moved pane still carries its verdict, in its
# header when it lands in a split and as the end-of-session card when it
# lands alone.
#
# "Broadcast" is the status-bar segment a SPLIT tab shows and a lone one
# does not, so it doubles as "which tab is on screen": after the drop it
# proves the view followed the pane into the destination split (the chip
# under a release is a button, and a button does not fire on a release
# whose press began elsewhere, so the app has to select it), and after a
# break-out its absence proves the new lone tab is the one displayed.
#
# Coordinates: (300, 54) is the left pane's header pick area (between
# the label and the controls); (470, 20) the third chip while the second
# is displayed; (460, 20) that same chip once it is the active, wider
# one; (40, 54) the left header's NAME; (566, 54) the left header's
# break-out button, the middle of its three controls.
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
# Kill the LEFT pane: it is the one every gesture below moves.
click (200, 300)
settle 300
type "exit"
type enter
settle 1400
expect "bash (default) (disconnected)"
expect "Broadcast"
# A third tab to drop onto, then back to the split.
click (438, 11)
expect "Local Shell"
click "Local Shell"
settle 1200
absent "Broadcast"
click (283, 20)
settle 800
expect "Broadcast"
# Drag the dead pane by its header onto the third tab's chip. The two
# moves clear iced's 10 px drag deadband before the strip is reached.
press (300, 54)
settle 200
move (340, 70)
settle 200
move (470, 20)
settle 400
release (470, 20)
settle 900
# The view followed the pane into the destination split, and the pane
# arrived whole: its header still says how its session ended.
expect "Broadcast"
expect "bash (default) (disconnected)"
screenshot pane-out-of-grid-dropped
# Tab menu: the arriving pane took the focus, so the row breaks it out
# again, into a lone tab of its own where its card answers for it.
click right (460, 20)
settle 300
expect "Move to a new tab"
click "Move to a new tab"
settle 900
absent "Broadcast"
expect "Session ended"
# Header menu: right-click on the pane's NAME raises its whole menu,
# whatever the terminal does with a right-click.
type ctrl+shift+d
settle 600
click "Local Shell"
settle 1200
expect "Broadcast"
click right (40, 54)
settle 300
expect "Move to a new tab"
type escape
settle 300
# The chord acts on the focused pane, which the right-click just made
# the dead one.
type ctrl+alt+p
settle 900
absent "Broadcast"
expect "Session ended"
# The header button, on the same pane after one more split.
type ctrl+shift+d
settle 600
click "Local Shell"
settle 1200
expect "Broadcast"
click (566, 54)
settle 900
absent "Broadcast"
expect "Session ended"
screenshot pane-out-of-grid-end
