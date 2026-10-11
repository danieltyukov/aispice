# Written by aispice. Extracts a cell of a GDS file with Magic and the
# PDK's tech file and writes a SPICE subcircuit. Inputs come as environment
# variables, each used as one Tcl word and never evaluated:
#   AISPICE_GDS      the GDS file
#   AISPICE_CELL     the cell to extract
#   AISPICE_OUT      the SPICE file to write
#   AISPICE_RESIST   1 to add resistor networks (extresist)
#   AISPICE_UNLABEL  a net whose labels are removed first, or empty
#   AISPICE_CTHRESH  smallest capacitor kept, fF

gds read $env(AISPICE_GDS)
load $env(AISPICE_CELL)
select top cell

# Magic's extresist fails on a substrate net that carries a label, so the
# caller names that net here and puts it back in the netlist afterwards.
if {$env(AISPICE_UNLABEL) ne ""} {
    for {set i 0} {$i < 64} {incr i} {
        box values -999990 -999990 -999980 -999980
        findlabel $env(AISPICE_UNLABEL)
        if {[box values] eq "-999990 -999990 -999980 -999980"} {
            break
        }
        erase labels
    }
}

extract do local
extract do capacitance
extract do coupling
if {$env(AISPICE_RESIST) eq "1"} {
    extract do resistance
}
extract all
if {$env(AISPICE_RESIST) eq "1"} {
    # Every net gets its network, however short: an analog block is small
    # and its nets are what the post-layout numbers are about.
    extresist threshold 0
    extresist mindelay 0
    extresist all
    ext2spice extresist on
}
ext2spice cthresh $env(AISPICE_CTHRESH)
ext2spice subcircuit top on
ext2spice format ngspice
ext2spice -o $env(AISPICE_OUT)
puts "aispice: extracted"
quit -noprompt
