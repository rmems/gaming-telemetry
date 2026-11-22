// SPDX-License-Identifier: MIT OR Apache-2.0
//
// gaming-telemetry CPU sensor sidecar for Windows.
//
// Reads CPU telemetry through LibreHardwareMonitorLib — the same sensor stack
// the standalone LHM app uses, embedded so the collector owns the lifecycle:
// it spawns this process at capture start and kills it at the end. There is
// no GUI to launch, no shared-memory option to enable, and no web server to
// run. Kernel access goes through the PawnIO driver LHM swapped WinRing0 for
// (0.9.5+), which must be installed once, and the process must run elevated —
// the collector inherits its own token, so an elevated collector is enough.
//
// Protocol: one JSON object per sample on stdout, one line each:
//   {"ts_ms":1763340000000,"tctl_c":61.5,"ccd1_c":68.0,"ccd2_c":null,"package_power_w":142.5}
// A sensor that is absent or unreadable is emitted as null — never 0 — so a
// missing reading is indistinguishable from a real idle CPU only on purpose.
// Diagnostics go to stderr; stdout carries samples only.
//
// Exit codes: 0 clean (or broken pipe — the collector went away), 3 not
// elevated, 4 no usable sensor data (driver missing, unsupported hardware).

using System.Security.Principal;
using System.Text.Json;
using System.Text.Json.Serialization;
using LibreHardwareMonitor.Hardware;

const int SampleIntervalMs = 500;
// LHM enumerates hardware and loads the PawnIO modules within a few seconds;
// if none of the four target sensors have even appeared by this deadline the
// host cannot produce telemetry (PawnIO not installed, unsupported CPU) and
// waiting longer only delays the collector's fallbacks.
const int NoSensorDeadlineMs = 15_000;
const int ExitNotElevated = 3;
const int ExitNoSensors = 4;

// Needles mirror the collector's own selectors (src/cpu/lhm.rs): Tctl/Tdie
// first, Intel's "CPU Package" as the fallback; the digit guard keeps "ccd1"
// from matching "ccd10" on many-CCD parts.
string[] tctlNeedles = ["tctl", "package"];
string[] ccd1Needles = ["ccd1"];
string[] ccd2Needles = ["ccd2"];
string[] powerNeedles = ["package"];

if (!IsAdministrator())
{
    Console.Error.WriteLine(
        "gaming-telemetry-cpu-sidecar: administrator rights are required — the " +
        "PawnIO sensor driver only serves elevated processes. Run the collector " +
        "from an elevated terminal.");
    return ExitNotElevated;
}

if (!PawnIoPresent())
{
    Console.Error.WriteLine(
        "gaming-telemetry-cpu-sidecar: the PawnIO driver is not installed — " +
        "LibreHardwareMonitor (0.9.5+) cannot read sensors without it. " +
        "See https://pawnio.eu");
    return ExitNoSensors;
}

// Self-reap when the collector dies: its stdin is a pipe it holds open for
// exactly this purpose, so EOF means the parent is gone — crash, Task
// Manager kill, console close — where its Drop cleanup never runs. The
// broken-pipe catch in the write loop cannot be relied on: .NET's Windows
// console stream treats ERROR_BROKEN_PIPE as success and drops the write.
new Thread(static () =>
{
    try
    {
        while (Console.In.Read() != -1) { }
    }
    catch (IOException) { }
    Environment.Exit(0);
})
{ IsBackground = true }.Start();

Computer computer;
try
{
    computer = new Computer { IsCpuEnabled = true };
    computer.Open();
}
catch (Exception e)
{
    Console.Error.WriteLine(
        $"gaming-telemetry-cpu-sidecar: LibreHardwareMonitor failed to open: {e.Message}");
    return ExitNoSensors;
}

var cpuHardware = computer.Hardware
    .Where(h => h.HardwareType == HardwareType.Cpu)
    .SelectMany(WithSubHardware)
    .ToList();

if (cpuHardware.Count == 0)
{
    Console.Error.WriteLine(
        "gaming-telemetry-cpu-sidecar: no CPU hardware node was enumerated. " +
        "Is the PawnIO driver installed? See https://pawnio.eu");
    return ExitNoSensors;
}

var deadline = Environment.TickCount64 + NoSensorDeadlineMs;
try
{
    while (true)
    {
        foreach (var hw in cpuHardware)
            hw.Update();

        var tctl = Pick(cpuHardware, SensorType.Temperature, tctlNeedles);
        var ccd1 = Pick(cpuHardware, SensorType.Temperature, ccd1Needles);
        var ccd2 = Pick(cpuHardware, SensorType.Temperature, ccd2Needles);
        var power = Pick(cpuHardware, SensorType.Power, powerNeedles);

        if (!tctl.Found && !ccd1.Found && !ccd2.Found && !power.Found)
        {
            if (Environment.TickCount64 >= deadline)
            {
                Console.Error.WriteLine(
                    "gaming-telemetry-cpu-sidecar: no CPU temperature or package-power " +
                    "sensors appeared — the PawnIO driver is not installed or this CPU " +
                    "is unsupported. See https://pawnio.eu");
                return ExitNoSensors;
            }
        }
        else
        {
            var sample = new Sample(
                DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
                Finite(tctl.Value),
                Finite(ccd1.Value),
                Finite(ccd2.Value),
                Finite(power.Value));
            try
            {
                Console.Out.WriteLine(JsonSerializer.Serialize(sample));
                Console.Out.Flush();
            }
            catch (IOException)
            {
                return 0; // stdout went away: the collector is gone.
            }
        }

        Thread.Sleep(SampleIntervalMs);
    }
}
finally
{
    computer.Close();
}

/// First sensor matching a needle, needles tried in order. `Found` tracks the
/// name match even when the value itself is transiently null.
static (bool Found, float? Value) Pick(
    IReadOnlyList<IHardware> hardware,
    SensorType type,
    string[] needles)
{
    foreach (var needle in needles)
    {
        var sensor = hardware
            .SelectMany(hw => hw.Sensors)
            .FirstOrDefault(s => s.SensorType == type && ContainsToken(s.Name, needle));
        if (sensor is not null)
            return (true, sensor.Value);
    }
    return (false, null);
}

/// A reading that is not finite is "not measured" — NaN or infinity in a
/// JSON number would corrupt the line, so it degrades to null, never 0.
static float? Finite(float? value) => value is { } v && float.IsFinite(v) ? v : null;

/// `needle` occurs in `hay` (ordinal, case-insensitive) and is not immediately
/// followed by a digit — port of the collector's `contains_token`.
static bool ContainsToken(string hay, string needle)
{
    for (var i = 0; i + needle.Length <= hay.Length; i++)
    {
        if (string.Compare(hay, i, needle, 0, needle.Length, StringComparison.OrdinalIgnoreCase) != 0)
            continue;
        var next = i + needle.Length;
        if (next >= hay.Length || !char.IsAsciiDigit(hay[next]))
            return true;
    }
    return false;
}

static IEnumerable<IHardware> WithSubHardware(IHardware hw)
{
    yield return hw;
    foreach (var sub in hw.SubHardware)
        yield return sub;
}

/// The device object PawnIO installs. Checking it directly matters: with the
/// driver absent LHM still opens and still "reads" sensors — its register
/// calls return zero-filled buffers that surface as a plausible 0 °C / 0 W —
/// so sensor output cannot be trusted to tell "no driver" from "cold CPU".
/// Any open result other than file-not-found means the device exists.
static bool PawnIoPresent()
{
    try
    {
        using var stream = new FileStream(
            @"\\.\PawnIO", FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
        return true;
    }
    catch (Exception e) when (e is FileNotFoundException or DirectoryNotFoundException)
    {
        return false;
    }
    catch
    {
        return true;
    }
}

static bool IsAdministrator() =>
    new WindowsPrincipal(WindowsIdentity.GetCurrent()).IsInRole(WindowsBuiltInRole.Administrator);

sealed record Sample(
    [property: JsonPropertyName("ts_ms")] long TsMs,
    [property: JsonPropertyName("tctl_c")] float? TctlC,
    [property: JsonPropertyName("ccd1_c")] float? Ccd1C,
    [property: JsonPropertyName("ccd2_c")] float? Ccd2C,
    [property: JsonPropertyName("package_power_w")] float? PackagePowerW);
