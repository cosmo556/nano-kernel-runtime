# NKR (Nano-Kernel Runtime) - Guidelines

## Critical: Token Efficiency
- **No Pleasantries:** Respond directly. Skip "I understand", "Sure", or "I've updated".
- **Diffs Only:** Provide only the changed lines or `unified diff` format. Never rewrite entire files.
- **Concise:** Use bullet points. Explain logic only if it's a complex ACPI/IOAPIC refactor.

## Technical Context (Low-Level)
- **Environment:** Freestanding C and `no_std` Rust. No standard libraries.
- **Focus:** ACPI, IOAPIC, PCI Passthrough, and initramfs optimization.
- **Memory:** Prioritize manual memory management and direct hardware addressing.
- **Tooling:** Use `make build`, `make install`, and `qemu` commands for testing.

## Agent Behavior
- **Check Before Read:** Use `ls -R` or `find` to locate files before reading them to avoid unnecessary `cat` calls.
- **Terminal Priority:** Use bash commands to verify build status before suggesting code changes.
- **Background Tasks:** If a build takes >5 mins, inform and wait for user input.