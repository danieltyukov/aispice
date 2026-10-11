# Security policy

aispice runs on your machine, reads and writes circuit files in the project folder you open, starts simulators, and sends prompts and circuit contents to the model provider you configure. It stores your provider API keys. Problems in those areas matter most.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately through GitHub security advisories: https://github.com/danieltyukov/aispice/security/advisories/new. Include the version (`aispice --version`), the operating system, the steps to reproduce and what an attacker could gain.

## Scope

In scope:

- API keys appearing anywhere other than the OS keychain or the 0600 fallback file: logs, tool output, error messages, session files, crash reports.
- Any way a tool can read or write outside the opened project folder, including through symlinks, `..` segments or absolute paths in model output.
- Prompt injection that makes aispice do something the user did not ask for: a comment in a schematic, a model file or a simulator log steering the agent into writing files elsewhere, running a different program, or sending data to a host other than the configured provider.
- Command injection through file names, component values or directives reaching a shell. aispice never builds shell command strings; any case where it does is a bug.
- The desktop app's IPC surface and content security policy.
- The Spectre remote backend: anything that lets model output change the SSH host or the remote command.
- Compiled device models: any way a netlist, a project file or model output gets a simulator to load an OSDI file, Xyce plugin or code model from anywhere but a PDK folder the user registered with `aispice pdk add`.
- The release pipeline.

Out of scope:

- Vulnerabilities in LTspice, ngspice, Xyce, Spectre or a model provider. Report those to their maintainers.
- Issues that require an attacker who already controls the user's OS account.
- The model giving wrong engineering advice. That is a quality problem; please open a normal issue with the circuit.

## What to expect

You should get an acknowledgement within seven days. Confirmed problems are fixed in a patch release and described in `CHANGELOG.md` and the advisory. Only the latest release receives fixes.
