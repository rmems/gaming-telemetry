// SPDX-License-Identifier: MIT OR Apache-2.0

// The whole helper is Windows-only (win-x64, self-contained): it exists to
// reach Windows sensor drivers. Declaring it once here keeps the CA1416
// analyzer from flagging every Windows API call site.
[assembly: System.Runtime.Versioning.SupportedOSPlatform("windows")]
