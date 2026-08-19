# Third-Party Notices

This project depends on the Rust packages listed below. The inventory is derived
from the resolved packages in `Cargo.lock`; it excludes the workspace package
itself. Because a Cargo lockfile also records some feature-optional packages,
the inventory is intentionally broader than the crates compiled by the default
build. License expressions are the SPDX expressions declared in each package's
published Cargo metadata. In an `OR` expression, the recipient may use the
package under any one of the listed licenses, subject to that license's terms.

Every package below offers the MIT License as a redistribution option. The
project chooses that option for the bundled binary and reproduces the required
notices and license terms below. The upstream links remain the authoritative
source for package-specific history and alternative license options.

| Package | Version | Relationship | Declared license | Upstream |
| --- | ---: | --- | --- | --- |
| `bitflags` | 2.13.1 | Transitive | MIT OR Apache-2.0 | [Repository / homepage](https://github.com/bitflags/bitflags) |
| `block2` | 0.6.2 | Direct (optional; enabled by the default feature set) | MIT | [Repository](https://github.com/madsmtm/objc2) |
| `dispatch2` | 0.3.1 | Direct | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `libc` | 0.2.189 | Direct | MIT OR Apache-2.0 | [Repository](https://github.com/rust-lang/libc) |
| `objc2` | 0.6.4 | Direct | MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-app-kit` | 0.3.2 | Direct | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-application-services` | 0.3.2 | Direct | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-core-foundation` | 0.3.2 | Direct | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-core-graphics` | 0.3.2 | Direct | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-encode` | 4.1.0 | Transitive | MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-foundation` | 0.3.2 | Direct | MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-io-surface` | 0.3.2 | Transitive (optional; not enabled by default) | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-screen-capture-kit` | 0.3.2 | Direct (optional; enabled by the default feature set) | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-security` | 0.3.2 | Transitive (optional; not enabled by default) | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |
| `objc2-service-management` | 0.3.2 | Direct | Zlib OR Apache-2.0 OR MIT | [Repository](https://github.com/madsmtm/objc2) |

For the exact source corresponding to a locked package, use its name, version,
and checksum from `Cargo.lock` to obtain the package archive from
[crates.io](https://crates.io/).

## MIT notices and terms

The following notices cover the selected MIT terms for the dependency groups:

- `bitflags`: Copyright (c) 2014 The Rust Project Developers.
- `libc`: Copyright (c) The Rust Project Developers.
- `block2`, `dispatch2`, `objc2`, `objc2-encode`, and the `objc2-*` framework
  crates: Copyright Mads Marquart, Mary, and contributors, as applicable.

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
