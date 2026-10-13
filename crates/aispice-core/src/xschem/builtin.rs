//! Short definitions of the xschem symbols aispice reads and writes most, for
//! machines without xschem or the PDK installed.
//!
//! Each one keeps what netlisting and wiring need, written for aispice: the
//! symbol's `type`, `format` and `template`, and its pins (name, position,
//! order). The body is a plain box of about the drawing's size. Installed
//! symbol files always take precedence; a gated test checks these against
//! xschem's own library and IHP's `sg13g2_pr` (pins, `format`, `template`).

/// `(reference, definition)`. The `devices/` set is also found by bare name
/// (`res.sym`), as xschem's library path makes it.
pub(super) const SYMBOLS: &[(&str, &str)] = &[
    (
        "devices/res.sym",
        r#"K {type=resistor
format="@name @pinlist @value m=@m"
template="name=R1
value=1k
footprint=1206
device=resistor
m=1"}
B 4 -7.5 -20 7.5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout propag=1 pinnumber=1 goto=1}
B 5 -2.5 27.5 2.5 32.5 {name=M dir=inout propag=0 pinnumber=2 goto=0}
"#,
    ),
    (
        "devices/capa.sym",
        r#"K {type=capacitor
format="@name @pinlist @value m=@m"
template="name=C1
m=1
value=1p
footprint=1206
device=\\"ceramic capacitor\\""}
B 4 -10 -5 10 5 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout pinnumber=2}
"#,
    ),
    (
        "devices/capa-2.sym",
        r#"K {type=polarized_capacitor
format="@name @pinlist @value m=@m"
template="name=C1
m=1
value=1p
footprint=1206
device=polarized_capacitor
}
B 4 -10 -5 10 5 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout propag=1 pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout propag=0 pinnumber=2}
"#,
    ),
    (
        "devices/ind.sym",
        r#"K {type=inductor
format="@name @pinlist @value m=@m"
template="name=L1
m=1
value=1n
footprint=1206
device=inductor"}
B 4 -5 -20 5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout propag=1 pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout propag=0 pinnumber=2}
"#,
    ),
    (
        "devices/vsource.sym",
        r#"K {type=vsource
format="tcleval([expr \{@savecurrent ? \\"@name @pinlist @value
.save I( ?1 @name )\\" : \\"@name @pinlist @value \\"\}])"
template="name=V1 value=3 savecurrent=false"}
B 4 -15 -15 15 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/isource.sym",
        r#"K {type=isource
format="@name @pinlist @value"
template="name=I0 value=1m"}
B 4 -15 -15 15 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/ammeter.sym",
        r#"K {type=ammeter
format="tcleval([expr \{@savecurrent ? \\"@name @pinlist 0
.save I( ?1 @name )\\" : \\"@name @pinlist 0\\"\}])"
template="name=Vmeas savecurrent=true"}
B 4 -10 -15 10 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=plus dir=inout propag=1}
B 5 -2.5 27.5 2.5 32.5 {name=minus dir=inout propag=0}
"#,
    ),
    (
        "devices/diode.sym",
        r#"K {type=diode
format="@name @pinlist @model area=@area"
template="name=D1 model=D1N914 area=1"}
B 4 -10 -10 10 10 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout pinnumber=1 propag=1 goto=1}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout pinnumber=2 goto=0}
"#,
    ),
    (
        "devices/npn.sym",
        r#"K {type=npn
format="@name @pinlist  @model area=@area m=@m"
template="name=Q1
model=MMBT2222
device=MMBT2222
footprint=SOT23
area=1
m=1"}
B 4 -10 -20 20 20 {}
B 5 17.5 -32.5 22.5 -27.5 {name=C dir=inout pinnumber=3}
B 5 -22.5 -2.5 -17.5 2.5 {name=B dir=in pinnumber=1}
B 5 17.5 27.5 22.5 32.5 {name=E dir=inout pinnumber=2}
"#,
    ),
    (
        "devices/pnp.sym",
        r#"K {type=pnp
format="@spiceprefix@name @pinlist @model area=@area m=@m"
template="name=Q1
model=Q2N2907
device=2N2907
footprint=TO92
area=1
m=1"}
B 4 -10 -20 20 20 {}
B 5 17.5 27.5 22.5 32.5 {name=C dir=inout pinnumber=3}
B 5 -22.5 -2.5 -17.5 2.5 {name=B dir=in pinnumber=1}
B 5 17.5 -32.5 22.5 -27.5 {name=E dir=inout pinnumber=2}
"#,
    ),
    (
        "devices/nmos4.sym",
        r#"K {type=nmos
format="@spiceprefix@name @pinlist @model w=@w l=@l @extra m=@m"
template="name=M1 model=nmos w=5u l=0.18u del=0 m=1"}
B 4 -10 -22.5 20 22.5 {}
B 5 17.5 -32.5 22.5 -27.5 {name=d dir=inout}
B 5 -22.5 -2.5 -17.5 2.5 {name=g dir=in}
B 5 17.5 27.5 22.5 32.5 {name=s dir=inout}
B 5 17.5 -2.5 22.5 2.5 {name=b dir=in}
"#,
    ),
    (
        "devices/pmos4.sym",
        r#"K {type=pmos
format="@spiceprefix@name @pinlist @model w=@w l=@l @extra m=@m"
template="name=M1 model=pmos w=5u l=0.18u del=0 m=1"}
B 4 -10 -22.5 20 22.5 {}
B 5 17.5 27.5 22.5 32.5 {name=d dir=inout}
B 5 -22.5 -2.5 -17.5 2.5 {name=g dir=in}
B 5 17.5 -32.5 22.5 -27.5 {name=s dir=inout}
B 5 17.5 -2.5 22.5 2.5 {name=b dir=in}
"#,
    ),
    (
        "devices/vcvs.sym",
        r#"K {type=vcvs
format="@name @pinlist @value"
template="name=E1 value=3"}
B 4 -40 -20 15 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
B 5 -42.5 -22.5 -37.5 -17.5 {name=cp dir=in}
B 5 -42.5 17.5 -37.5 22.5 {name=cm dir=in}
"#,
    ),
    (
        "devices/vccs.sym",
        r#"K {type=vccs
format="@name @pinlist @value"
template="name=G1 value=1e-6"}
B 4 -40 -20 15 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
B 5 -42.5 -22.5 -37.5 -17.5 {name=cp dir=in}
B 5 -42.5 17.5 -37.5 22.5 {name=cm dir=in}
"#,
    ),
    (
        "devices/cccs.sym",
        r#"K {type=vcvs
format="@name @pinlist @vnam @value"
template="name=F1 vnam=v1 value=1"}
B 4 -15 -20 15 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=in}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=in}
"#,
    ),
    (
        "devices/ccvs.sym",
        r#"K {type=vcvs
format="@name @pinlist @vnam @value"
template="name=H1 vnam=v1 value=1"}
B 4 -15 -20 15 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/bsource.sym",
        r#"K {type=source
format="@name @pinlist @VAR = @FUNC "
template="name=B1 VAR=I FUNC=\\"pwl(V(plus,minus),0,0, 1,10m, 2, 100m)\\""}
B 4 -15 -15 15 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/lab_pin.sym",
        r#"K {type=label
format="*.alias @lab"
template="name=p1 sig_type=std_logic lab=xxx"
net_name=true}
B 5 -1.25 -1.25 1.25 1.25 {name=p dir=in}
"#,
    ),
    (
        "devices/lab_wire.sym",
        r#"K {type=label
format="*.alias @lab"
template="name=p1 sig_type=std_logic lab=xxx"
net_name=true}
B 5 -1.25 -1.25 1.25 1.25 {name=p dir=in}
"#,
    ),
    (
        "devices/ipin.sym",
        r#"K {type=ipin
format="*.ipin @lab"
template="name=p1 lab=xxx"
net_name=true}
B 5 -1.25 -1.25 1.25 1.25 {name=p dir=out}
"#,
    ),
    (
        "devices/opin.sym",
        r#"K {type=opin
format="*.opin @lab"
template="name=p1 lab=xxx"
net_name=true}
B 5 -1.25 -1.25 1.25 1.25 {name=p dir=in}
"#,
    ),
    (
        "devices/iopin.sym",
        r#"K {type=iopin
format="*.iopin @lab"
template="name=p1 lab=xxx"
net_name=true}
B 5 -1.25 -1.25 1.25 1.25 {name=p dir=inout}
"#,
    ),
    (
        "devices/gnd.sym",
        r#"K {type=label
function0="L"
global=true
format="*.alias @lab"
template="name=l1 lab=GND"}
B 5 -2.5 -2.5 2.5 2.5 {name=p dir=inout goto=0}
"#,
    ),
    (
        "devices/vdd.sym",
        r#"K {type=label
function0="H"
global=true
format="*.alias @lab"
template="name=l1 lab=VDD"}
B 5 -2.5 -2.5 2.5 2.5 {name=p dir=inout verilog_type=wire goto=0}
"#,
    ),
    (
        "devices/noconn.sym",
        r#"K {type=noconn
format="* noconn @#0:net_name"
template="name=l1"}
B 5 -1.25 -1.25 1.25 1.25 {name=p dir=inout}
"#,
    ),
    (
        "devices/code.sym",
        r#"G {type=netlist_commands
template="name=s1 only_toplevel=false value=blabla"
format="
@value
"}
"#,
    ),
    (
        "devices/code_shown.sym",
        r#"K {type=netlist_commands
template="name=s1 only_toplevel=false value=blabla"
format="
@value
"}
"#,
    ),
    (
        "devices/netlist.sym",
        r#"G {type=netlist_commands
template="name=s1 value=blabla"
format="
@value
"}
"#,
    ),
    (
        "devices/netlist_not_shown.sym",
        r#"G {type=netlist_commands
template="name=s1 only_toplevel=false value=blabla"
format="
@value
"}
"#,
    ),
    (
        "devices/simulator_commands.sym",
        r#"K {type=netlist_commands
template="name=COMMANDS
simulator=ngspice
only_toplevel=false
value=\\"
* ngspice commands
\\""
format="tcleval(
[if \{[ sim_is_@simulator ]\} \{ return \{
\\\\@value
\}\}]
)"}
"#,
    ),
    (
        "devices/simulator_commands_shown.sym",
        r#"K {type=netlist_commands
template="name=COMMANDS
simulator=ngspice
only_toplevel=false
value=\\"
* ngspice commands
\\""
format="tcleval(
[if \{[ sim_is_@simulator ]\} \{ return \{
\\\\@value
\}\}]
)"}
"#,
    ),
    (
        "devices/title.sym",
        r#"K {type=logo
template="name=l1 author=\\"Stefan Schippers\\""
verilog_ignore=true
vhdl_ignore=true
spice_ignore=true
tedax_ignore=true}
"#,
    ),
    (
        "devices/launcher.sym",
        r#"K {type=launcher
format="** @descr : @url"
verilog_ignore=true
vhdl_ignore=true
spice_ignore=true
template="name=h1
descr=google
url=www.google.com"}
"#,
    ),
    (
        "sg13g2_pr/sg13_lv_nmos.sym",
        r#"K {type=nmos
format="@spiceprefix@name @pinlist @model w=@w l=@l ng=@ng m=@m"
template="name=M1
l=0.13u
w=0.15u
ng=1
m=1
model=sg13_lv_nmos
spiceprefix=X
"}
B 4 -10 -22.5 20 22.5 {}
B 5 17.5 -32.5 22.5 -27.5 {name=D dir=inout}
B 5 -22.5 -2.5 -17.5 2.5 {name=G dir=in}
B 5 17.5 27.5 22.5 32.5 {name=S dir=inout}
B 5 19.921875 -0.078125 20.078125 0.078125 {name=B dir=in}
"#,
    ),
    (
        "sg13g2_pr/sg13_lv_pmos.sym",
        r#"K {type=pmos
format="@spiceprefix@name @pinlist @model w=@w l=@l ng=@ng m=@m"
template="name=M1
l=0.13u
w=0.15u
ng=1
m=1
model=sg13_lv_pmos
spiceprefix=X
"}
B 4 -10 -22.5 20 22.5 {}
B 5 17.5 27.5 22.5 32.5 {name=D dir=inout}
B 5 -22.5 -2.5 -17.5 2.5 {name=G dir=in}
B 5 17.5 -32.5 22.5 -27.5 {name=S dir=inout}
B 5 19.921875 -0.078125 20.078125 0.078125 {name=B dir=in}
"#,
    ),
    (
        "sg13g2_pr/sg13_hv_nmos.sym",
        r#"K {type=nmos
format="@spiceprefix@name @pinlist @model w=@w  l=@l ng=@ng m=@m"
template="name=M1
l=0.45u
w=0.3u
ng=1
m=1
model=sg13_hv_nmos
spiceprefix=X
"}
B 4 -10 -22.5 20 22.5 {}
B 5 17.5 -32.5 22.5 -27.5 {name=D dir=inout}
B 5 -22.5 -2.5 -17.5 2.5 {name=G dir=in}
B 5 17.5 27.5 22.5 32.5 {name=S dir=inout}
B 5 19.921875 -0.078125 20.078125 0.078125 {name=B dir=in}
"#,
    ),
    (
        "sg13g2_pr/sg13_hv_pmos.sym",
        r#"K {type=pmos
format="@spiceprefix@name @pinlist @model w=@w l=@l ng=@ng m=@m"
template="name=M1
l=0.4u
w=0.3u
ng=1
m=1
model=sg13_hv_pmos
spiceprefix=X
"}
B 4 -10 -22.5 20 22.5 {}
B 5 17.5 27.5 22.5 32.5 {name=D dir=inout}
B 5 -22.5 -2.5 -17.5 2.5 {name=G dir=in}
B 5 17.5 -32.5 22.5 -27.5 {name=S dir=inout}
B 5 19.921875 -0.078125 20.078125 0.078125 {name=B dir=in}
"#,
    ),
    (
        "sg13g2_pr/rsil.sym",
        r#"K {type=res
format="@spiceprefix@name @pinlist @body @model w=@w l=@l m=@m b=@b"
template="name=R1
w=0.5e-6
l=0.5e-6
model=rsil
body=sub!
spiceprefix=X
b=0
m=1
value=\\"expr_eng(  ( 9.0e-6 / @w + 7.0 * ( (@b + 1)* @l + ( 1.081*( @w + 1.0e-8 ) + 0.18e-6 )*@b ) / ( @w + 1.0e-8 ) ) / @m  )\\"
"}
B 4 -7.5 -20 7.5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout propag=1 sim_pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=M dir=inout propag=0 sim_pinnumber=2}
"#,
    ),
    (
        "sg13g2_pr/rppd.sym",
        r#"K {type=res
format="@spiceprefix@name @pinlist @body @model w=@w l=@l m=@m b=@b"
template="name=R1
w=0.5e-6
l=0.5e-6
model=rppd
body=sub!
spiceprefix=X
b=0
m=1
value=\\"expr_eng(  ( 70.0e-6 / @w + 260.0 * ( (@b + 1)* @l + ( 1.081*( @w + 6.0e-9 ) + 0.18e-6 )*@b ) / ( @w + 6.0e-9 ) ) / @m  )\\"
"}
B 4 -7.5 -20 7.5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout propag=1 sim_pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=M dir=inout propag=0 sim_pinnumber=2}
"#,
    ),
    (
        "sg13g2_pr/rhigh.sym",
        r#"K {type=res
format="@spiceprefix@name @pinlist @body @model w=@w l=@l m=@m b=@b"
template="name=R1
w=0.5e-6
l=0.96e-6
model=rhigh
body=sub!
spiceprefix=X
b=0
m=1
value=\\"expr_eng(  ( 1.6e-4 / @w + 1360.0 * ( (@b + 1)* @l + ( 1.081*( @w - 0.04e-6 ) + 0.18e-6 )*@b ) / ( @w - 0.04e-6 ) ) / @m  )\\"
"}
B 4 -7.5 -20 7.5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout propag=1 sim_pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=M dir=inout propag=0 sim_pinnumber=2}
"#,
    ),
    (
        "sg13g2_pr/cap_cmim.sym",
        r#"K {type=capacitor
format="@spiceprefix@name @pinlist @model w=@w l=@l m=@m"
template="name=C1
model=cap_cmim
w=7.0e-6
l=7.0e-6
m=1
spiceprefix=X"}
B 4 -10 -5 10 5 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=c0 dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=c1 dir=inout}
"#,
    ),
    (
        "sg13g2_pr/npn13G2.sym",
        r#"K {type=vertical_npn
format="@spiceprefix@name @pinlist @model Nx=@Nx"
template="name=Q1
model=npn13G2
spiceprefix=X
Nx=1
"}
B 4 -10 -20 20 20 {}
B 5 17.5 -32.5 22.5 -27.5 {name=C dir=inout sim_pinnumber=1}
B 5 -22.5 -2.5 -17.5 2.5 {name=B dir=in sim_pinnumber=2}
B 5 17.5 27.5 22.5 32.5 {name=E dir=inout sim_pinnumber=3}
B 5 17.5 -2.5 22.5 2.5 {name=S dir=in sim_pinnumber=4}
"#,
    ),
    (
        "sg13g2_pr/npn13G2l.sym",
        r#"K {type=vertical_npn
format="@spiceprefix@name @pinlist @model Nx=@Nx El=@El"
template="name=Q1
model=npn13G2l
spiceprefix=X
Nx=1
El=1.0
"}
B 4 -10 -20 20 20 {}
B 5 17.5 -32.5 22.5 -27.5 {name=C dir=inout sim_pinnumber=1}
B 5 -22.5 -2.5 -17.5 2.5 {name=B dir=in sim_pinnumber=2}
B 5 17.5 27.5 22.5 32.5 {name=E dir=inout sim_pinnumber=3}
B 5 17.5 -2.5 22.5 2.5 {name=S dir=in sim_pinnumber=4}
"#,
    ),
    (
        "sg13g2_pr/npn13G2v.sym",
        r#"K {type=vertical_npn
format="@spiceprefix@name @pinlist @model Nx=@Nx El=@El"
template="name=Q1
model=npn13G2v
spiceprefix=X
Nx=1
El=1.0
"}
B 4 -10 -20 20 20 {}
B 5 17.5 -32.5 22.5 -27.5 {name=C dir=inout sim_pinnumber=1}
B 5 -22.5 -2.5 -17.5 2.5 {name=B dir=in sim_pinnumber=2}
B 5 17.5 27.5 22.5 32.5 {name=E dir=inout sim_pinnumber=3}
B 5 17.5 -2.5 22.5 2.5 {name=S dir=in sim_pinnumber=4}
"#,
    ),
    (
        "sg13g2_pr/ptap1.sym",
        r#"K {type=res
format="tcleval(@spiceprefix@name @pinlist @model R=[ev \{  1.0 / ( 1.0 / ( 9.8e-10 / ( @w * @l ) ) + 1.0 / ( 9.8e-4 / ( 2.0 * ( @w + @l ) ) ) )\}] w=@w l=@l )"
template="name=R1
model=ptap1
spiceprefix=X
w=0.78e-6
l=0.78e-6
"}
B 4 -7.5 -20 7.5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout propag=1 }
B 5 -2.5 27.5 2.5 32.5 {name=M dir=inout propag=0 }
"#,
    ),
    (
        "sg13g2_pr/ntap1.sym",
        r#"K {type=res
format="tcleval(@spiceprefix@name @pinlist @model R=[ev \{  1.0 / ( 1.0 / ( 9.8e-10 / ( @w * @l ) ) + 1.0 / ( 9.8e-4 / ( 2.0 * ( @w + @l ) ) ) )\}] w=@w l=@l )"
template="name=R1
model=ntap1
spiceprefix=X
w=0.78e-6
l=0.78e-6
"}
B 4 -7.5 -20 7.5 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=P dir=inout propag=1}
B 5 -2.5 27.5 2.5 32.5 {name=M dir=inout propag=0}
"#,
    ),
    (
        "sg13g2_pr/dantenna.sym",
        r#"K {type=diode
format="@spiceprefix@name @pinlist @model  l=@l w=@w"
template="name=D1
model=dantenna
l=0.78u
w=0.78u
spiceprefix=X
"}
B 4 -10 -10 10 10 {}
B 5 -2.5 27.5 2.5 32.5 {name=d0 dir=inout sim_pinnumber=1}
B 5 -2.5 -32.5 2.5 -27.5 {name=d1 dir=inout sim_pinnumber=2}
"#,
    ),
    (
        "sg13g2_pr/dpantenna.sym",
        r#"K {type=diode
format="@spiceprefix@name @pinlist @model l=@l w=@w"
template="name=D1
model=dpantenna
l=0.78u
w=0.78u
spiceprefix=X
"}
B 4 -10 -10 10 10 {}
B 5 -2.5 27.5 2.5 32.5 {name=d0 dir=inout sim_pinnumber=1}
B 5 -2.5 -32.5 2.5 -27.5 {name=d1 dir=inout sim_pinnumber=2}
"#,
    ),
    (
        "sg13g2_pr/sub.sym",
        r#"K {type=label
function0="L"
global=true
format="*.alias @lab"
template="name=l1 lab=sub!"}
B 5 -2.5 -2.5 2.5 2.5 {name=p dir=inout goto=0}
"#,
    ),
    (
        "sg13g2_pr/annotate_fet_params.sym",
        r#"K {type=annotator
template="name=annot1 ref=M1"}
"#,
    ),
    (
        "sg13g2_pr/annotate_bip_params.sym",
        r#"K {type=annotator
template="name=annot1 ref=Q1"}
"#,
    ),
];

/// The built-in definition for a symbol reference, if there is one. The
/// `devices/` set also answers to its bare name.
pub(super) fn get(reference: &str) -> Option<(&'static str, &'static str)> {
    let r = reference.trim().trim_start_matches("./");
    SYMBOLS
        .iter()
        .find(|(name, _)| *name == r || name.strip_prefix("devices/") == Some(r))
        .copied()
}

/// The symbols above that xschem 3.4.5 to 3.4.8 write differently, as 3.4.8
/// has them. The built-ins are 3.4.4's, the release Ubuntu 24.04 packages;
/// the drift test accepts an installed symbol that matches either, and the
/// netlist tests show aispice netlists both alike.
#[cfg(test)]
pub(super) const LATER: &[(&str, &str)] = &[
    (
        "devices/vsource.sym",
        r#"K {type=vsource
format="@name @pinlist @value@savecurrent"
template="name=V1 value=3 savecurrent=false"}
B 4 -15 -15 15 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/ammeter.sym",
        r#"K {type=ammeter
format="@name @pinlist 0 @savecurrent"
template="name=Vmeas savecurrent=true spice_ignore=0"}
B 4 -10 -15 10 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=plus dir=inout propag=1}
B 5 -2.5 27.5 2.5 32.5 {name=minus dir=inout propag=0}
"#,
    ),
    (
        "devices/cccs.sym",
        r#"K {type=cccs
format="@name @pinlist @vnam @value"
template="name=F1 vnam=v1 value=1"}
B 4 -15 -20 15 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=in}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=in}
"#,
    ),
    (
        "devices/ccvs.sym",
        r#"K {type=ccvs
format="@name @pinlist @vnam @value"
template="name=H1 vnam=v1 value=1"}
B 4 -15 -20 15 20 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/bsource.sym",
        r#"K {type=source
format="@name @pinlist @VAR = @FUNC m=@m"
template="name=B1 VAR=I FUNC=\\"'pwl(V(plus,minus),0,0, 1,10m, 2, 100m)'\\" m=1"}
B 4 -15 -15 15 15 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout}
"#,
    ),
    (
        "devices/gnd.sym",
        r#"K {type=label
function0="L"
global=ground
format="*.alias @lab"
template="name=l1 lab=0"}
B 5 -2.5 -2.5 2.5 2.5 {name=p dir=inout goto=0}
"#,
    ),
    (
        "devices/capa-2.sym",
        r#"K {type=polarized_capacitor
format="@name @pinlist @value m=@m"
template="name=C1
m=1
value=1p
footprint=1206
device=polarized_capacitor"
}
B 4 -10 -5 10 5 {}
B 5 -2.5 -32.5 2.5 -27.5 {name=p dir=inout propag=1 pinnumber=1}
B 5 -2.5 27.5 2.5 32.5 {name=m dir=inout propag=0 pinnumber=2}
"#,
    ),
    (
        "devices/simulator_commands.sym",
        r#"K {type=netlist_commands
template="name=COMMANDS
simulator=ngspice
only_toplevel=false
value=\\"
* ngspice commands
\\""
format="tcleval(
[if \{[catch \{sim_is_@simulator \} retval] \} \{
  return \{\} \} elseif \{$retval\} \{ return \{
@value
\}
\} else \{ return \{\} \}
])"}
"#,
    ),
    (
        "devices/simulator_commands_shown.sym",
        r#"K {type=netlist_commands
template="name=COMMANDS
simulator=ngspice
only_toplevel=false
value=\\"
* ngspice commands
\\""
format="tcleval(
[if \{[catch \{sim_is_@simulator \} retval] \} \{
  return \{\} \} elseif \{$retval\} \{ return \{
@value
\}
\} else \{ return \{\} \}
])"}
"#,
    ),
];
