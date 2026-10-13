# Written by aispice. Writes the LEF abstract of a cell with Magic and the
# PDK's tech file: pins on their layers, the rest of the cell as
# obstructions (`lef write -hide`). Inputs come as environment variables,
# each used as one Tcl word and never evaluated:
#   AISPICE_GDS    the GDS file
#   AISPICE_CELL   the cell
#   AISPICE_LEF    the LEF file to write
#   AISPICE_PINS   the pins as `name:class:use` words separated by spaces;
#                  class is input, output or inout, use signal, clock,
#                  analog, power or ground

set name_rule {^[A-Za-z_][A-Za-z0-9_]{0,63}$}
set pins {}
foreach word [split $env(AISPICE_PINS) " "] {
    if {$word eq ""} {
        continue
    }
    set parts [split $word ":"]
    if {[llength $parts] != 3} {
        puts "aispice: bad pin entry"
        quit -noprompt
    }
    lassign $parts pin class use
    if {![regexp $name_rule $pin]
        || [lsearch -exact {input output inout} $class] < 0
        || [lsearch -exact {signal clock analog power ground} $use] < 0} {
        puts "aispice: bad pin entry"
        quit -noprompt
    }
    lappend pins [list $pin $class $use]
}

gds read $env(AISPICE_GDS)
load $env(AISPICE_CELL)
select top cell
foreach entry $pins {
    lassign $entry pin class use
    port $pin class $class
    port $pin use $use
}
lef write $env(AISPICE_LEF) -hide
puts "aispice: wrote the abstract"
quit -noprompt
