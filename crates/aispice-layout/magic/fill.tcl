# Written by aispice after IHP's generate_fill.py (its single-process mode):
# metal and active fill for a chip, with Magic and the PDK's GDS tech file,
# which keeps every layer as drawn. The chip is cut into 800 um tiles; each
# is flattened and written through the tech file's patternfill output
# style, which writes the fill shapes only; the tiles are then put together
# into one fill layout, `<cell>_fill_pattern`. Inputs come as environment
# variables, each used as one Tcl word and never evaluated:
#   AISPICE_GDS    the chip, with its seal ring
#   AISPICE_CELL   its top cell
#   AISPICE_OUT    the fill layout to write

drc off
crashbackups stop
locking disable
tech unlock *
scalegrid 1 2
units internal
box values 0 0 0 0
box size 800um 800um
set stepbox [box values]
set stepwidth [lindex $stepbox 2]
set stepheight [lindex $stepbox 3]

set project $env(AISPICE_CELL)
if {![regexp {^[A-Za-z_][A-Za-z0-9_]{0,127}$} $project]} {
    puts "aispice: bad cell name"
    quit -noprompt
}
set tile ${project}_fill_pattern

gds rescale false
gds drccheck no
gds read $env(AISPICE_GDS)
load $project -dereference
select top cell
expand
cif ostyle patternfill(tiled)

select top cell
catch {box values {*}[property FIXED_BBOX]}
set fullbox [box values]
set xmin [lindex $fullbox 0]
set ymin [lindex $fullbox 1]
set xmax [lindex $fullbox 2]
set ymax [lindex $fullbox 3]
set fullwidth [expr {$xmax - $xmin}]
set fullheight [expr {$ymax - $ymin}]
set xtiles [expr {int(ceil(($fullwidth + 0.0) / $stepwidth))}]
set ytiles [expr {int(ceil(($fullheight + 0.0) / $stepheight))}]
set xbase $xmin
set ybase $ymin

# No fill between the seal ring and the chip's edge, nor in its corners.
select top cell
select area EDGESEAL
set found [what -list]
if {[lindex $found 0] == "EDGESEAL"} {
    box select
    set sealbox [box values]
    set seallx [lindex $sealbox 0]
    set seally [lindex $sealbox 1]
    set sealux [lindex $sealbox 2]
    set sealuy [lindex $sealbox 3]
    select area GLASS
    box select
    set passivbox [box values]
    set xmin [lindex $passivbox 0]
    set ymin [lindex $passivbox 1]
    set xmax [lindex $passivbox 2]
    set ymax [lindex $passivbox 3]
    box values $xmin $ymin $seallx $ymax
    paint FILLBLOCK; paint DIFFBLOCK
    box values $xmin $ymin $xmax $seallx
    paint FILLBLOCK; paint DIFFBLOCK
    box values $sealux $ymin $xmax $ymax
    paint FILLBLOCK; paint DIFFBLOCK
    box values $xmin $sealuy $xmax $ymax
    paint FILLBLOCK; paint DIFFBLOCK
    box position $seallx $seally
    box size 21um 21um
    splitpaint sw FILLBLOCK,DIFFBLOCK
    box position $sealux $seally
    box move w 21um
    splitpaint se FILLBLOCK,DIFFBLOCK
    box position $seallx $sealuy
    box move s 21um
    splitpaint nw FILLBLOCK,DIFFBLOCK
    box position $sealux $sealuy
    box move sw 21um
    splitpaint ne FILLBLOCK,DIFFBLOCK
    box position $xmin $ymin
    spliterase sw FILLBLOCK
    box position $xmax $ymin
    box move w 21um
    spliterase se FILLBLOCK
    box position $xmin $ymax
    box move s 21um
    spliterase nw FILLBLOCK
    box position $xmax $ymax
    box move sw 21um
    spliterase ne FILLBLOCK
}

for {set y 0} {$y < $ytiles} {incr y} {
    for {set x 0} {$x < $xtiles} {incr x} {
        set xlo [expr {$xbase + $x * $stepwidth}]
        set ylo [expr {$ybase + $y * $stepheight}]
        set xhi [expr {$xlo + $stepwidth}]
        set yhi [expr {$ylo + $stepheight}]
        if {$xhi > $fullwidth} {set xhi $fullwidth}
        if {$yhi > $fullheight} {set yhi $fullheight}
        box values $xlo $ylo $xhi $yhi
        box grow c 1.6um
        flatten -dobox -nolabels ${tile}_${x}_$y
        load ${tile}_${x}_$y
        property GDS_FILE ""
        select top cell
        erase COMMENT
        box values $xlo $ylo $xhi $yhi
        paint COMMENT
        gds write ${tile}_${x}_$y.gds
        load $project
        cellname delete ${tile}_${x}_$y
    }
}

# The tiles, as views of the files just written, put together.
gds readonly true
gds rescale false
for {set y 0} {$y < $ytiles} {incr y} {
    for {set x 0} {$x < $xtiles} {incr x} {
        set xlo [expr {$xbase + $x * $stepwidth}]
        set ylo [expr {$ybase + $y * $stepheight}]
        set xhi [expr {$xlo + $stepwidth}]
        set yhi [expr {$ylo + $stepheight}]
        load ${tile}_${x}_$y -silent
        box values $xlo $ylo $xhi $yhi
        paint COMMENT
        property FIXED_BBOX "$xlo $ylo $xhi $yhi"
        property GDS_FILE ${tile}_${x}_${y}.gds
        property GDS_START 0
    }
}
load $tile -silent
for {set y 0} {$y < $ytiles} {incr y} {
    for {set x 0} {$x < $xtiles} {incr x} {
        box values 0 0 0 0
        getcell ${tile}_${x}_$y child 0 0
    }
}
cif *hier write disable
cif *array write disable
gds write $env(AISPICE_OUT)
puts "aispice: wrote the fill"
quit -noprompt
