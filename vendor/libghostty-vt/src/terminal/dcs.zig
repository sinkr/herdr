const std = @import("std");
const build_options = @import("terminal_options");
const assert = @import("../quirks.zig").inlineAssert;
const Allocator = std.mem.Allocator;
const terminal = @import("main.zig");
const DCS = terminal.DCS;

const log = std.log.scoped(.terminal_dcs);

/// DCS command handler. This should be hooked into a terminal.Stream handler.
/// The hook/put/unhook functions are meant to be called from the
/// terminal.stream dcsHook, dcsPut, and dcsUnhook functions, respectively.
pub const Handler = struct {
    state: State = .{ .inactive = {} },

    /// Maximum bytes any DCS command can take. This is to prevent
    /// malicious input from causing us to allocate too much memory.
    /// This is arbitrarily set to 1MB today, increase if needed.
    max_bytes: usize = 1024 * 1024,

    /// Maximum bytes a Sixel passthrough sequence can accumulate. Sixel
    /// image payloads are much larger than other DCS commands, so they
    /// get a dedicated, larger cap. Sequences that exceed the cap are
    /// dropped entirely (never forwarded truncated).
    max_bytes_sixel: usize = 8 * 1024 * 1024,

    pub fn deinit(self: *Handler) void {
        self.discard();
    }

    pub fn hook(self: *Handler, alloc: Allocator, dcs: DCS) ?Command {
        assert(self.state == .inactive);

        // Initialize our state to ignore in case of error
        self.state = .ignore;

        // Try to parse the hook.
        const hk_ = self.tryHook(alloc, dcs) catch |err| {
            log.info("error initializing DCS hook, will ignore hook err={}", .{err});
            return null;
        };
        const hk = hk_ orelse {
            log.info("unknown DCS hook: {}", .{dcs});
            return null;
        };

        self.state = hk.state;
        return hk.command;
    }

    const Hook = struct {
        state: State,
        command: ?Command = null,
    };

    fn tryHook(self: Handler, alloc: Allocator, dcs: DCS) !?Hook {
        return switch (dcs.intermediates.len) {
            0 => switch (dcs.final) {
                // Tmux control mode
                'p' => tmux: {
                    if (comptime !build_options.tmux_control_mode) {
                        log.debug("tmux control mode not enabled in build, ignoring", .{});
                        break :tmux null;
                    }

                    // Tmux control mode must start with ESC P 1000 p
                    if (dcs.params.len != 1 or dcs.params[0] != 1000) break :tmux null;

                    break :tmux .{
                        .state = .{
                            .tmux = .{
                                .max_bytes = self.max_bytes,
                                .buffer = try .initCapacity(
                                    alloc,
                                    128, // Arbitrary choice to limit initial reallocs
                                ),
                            },
                        },
                        .command = .{ .tmux = .enter },
                    };
                },

                // Sixel graphics (DCS Ps;Ps;Ps q). The payload is not
                // decoded; the full sequence is re-synthesized and buffered
                // so hosts can pass it through to attached clients.
                'q' => sixel: {
                    var buffer: std.Io.Writer.Allocating = try .initCapacity(
                        alloc,
                        256, // Arbitrary choice to limit initial reallocs
                    );
                    errdefer buffer.deinit();

                    // Re-synthesize the introducer so unhook can surface
                    // the complete sequence bytes.
                    try buffer.writer.writeAll("\x1bP");
                    for (dcs.params, 0..) |param, i| {
                        if (i > 0) try buffer.writer.writeByte(';');
                        try buffer.writer.print("{d}", .{param});
                    }
                    try buffer.writer.writeByte('q');

                    break :sixel .{ .state = .{ .sixel = buffer } };
                },

                else => null,
            },

            1 => switch (dcs.intermediates[0]) {
                '+' => switch (dcs.final) {
                    // XTGETTCAP
                    // https://github.com/mitchellh/ghostty/issues/517
                    'q' => .{
                        .state = .{
                            .xtgettcap = try .initCapacity(
                                alloc,
                                128, // Arbitrary choice
                            ),
                        },
                    },

                    else => null,
                },

                '$' => switch (dcs.final) {
                    // DECRQSS
                    'q' => .{ .state = .{
                        .decrqss = .{},
                    } },

                    else => null,
                },

                else => null,
            },

            else => null,
        };
    }

    /// Put a byte into the DCS handler. This will return a command
    /// if a command needs to be executed.
    pub fn put(self: *Handler, byte: u8) ?Command {
        return self.tryPut(byte) catch |err| {
            // On error we just discard our state and ignore the rest
            log.info("error putting byte into DCS handler err={}", .{err});
            self.discard();
            self.state = .ignore;
            return null;
        };
    }

    fn tryPut(self: *Handler, byte: u8) !?Command {
        switch (self.state) {
            .inactive,
            .ignore,
            => {},

            .tmux => |*tmux| if (comptime build_options.tmux_control_mode) {
                return .{
                    .tmux = (try tmux.put(byte)) orelse return null,
                };
            } else unreachable,

            .xtgettcap => |*list| {
                if (list.written().len >= self.max_bytes) {
                    return error.OutOfMemory;
                }

                try list.writer.writeByte(byte);
            },

            .sixel => |*list| {
                if (list.written().len >= self.max_bytes_sixel) {
                    return error.OutOfMemory;
                }

                try list.writer.writeByte(byte);
            },

            .decrqss => |*buffer| {
                if (buffer.len >= buffer.data.len) {
                    return error.OutOfMemory;
                }

                buffer.data[buffer.len] = byte;
                buffer.len += 1;
            },
        }

        return null;
    }

    pub fn unhook(self: *Handler) ?Command {
        // Note: we do NOT call deinit here on purpose because some commands
        // transfer memory ownership. If state needs cleanup, the switch
        // prong below should handle it.
        defer self.state = .inactive;

        return switch (self.state) {
            .inactive,
            .ignore,
            => null,

            .tmux => if (comptime build_options.tmux_control_mode) tmux: {
                self.state.deinit();
                break :tmux .{ .tmux = .exit };
            } else unreachable,

            .xtgettcap => |*list| xtgettcap: {
                // Note: purposely do not deinit our state here because
                // we copy it into the resulting command.
                const items = list.written();
                for (items, 0..) |b, i| items[i] = std.ascii.toUpper(b);
                break :xtgettcap .{ .xtgettcap = .{ .data = list.* } };
            },

            .sixel => |*list| sixel: {
                // Note: purposely do not deinit our state here because
                // we transfer ownership into the resulting command.
                list.writer.writeAll("\x1b\\") catch {
                    // Terminating the sequence failed (allocation); drop
                    // the whole emission rather than forward a truncated
                    // sequence.
                    var owned = list.*;
                    owned.deinit();
                    break :sixel null;
                };
                break :sixel .{ .sixel = .{ .data = list.* } };
            },

            .decrqss => |buffer| .{ .decrqss = switch (buffer.len) {
                0 => .none,
                1 => switch (buffer.data[0]) {
                    'm' => .sgr,
                    'r' => .decstbm,
                    's' => .decslrm,
                    else => .none,
                },
                2 => switch (buffer.data[0]) {
                    ' ' => switch (buffer.data[1]) {
                        'q' => .decscusr,
                        else => .none,
                    },
                    else => .none,
                },
                else => unreachable,
            } },
        };
    }

    fn discard(self: *Handler) void {
        self.state.deinit();
        self.state = .inactive;
    }
};

pub const Command = union(enum) {
    /// XTGETTCAP
    xtgettcap: XTGETTCAP,

    /// DECRQSS
    decrqss: DECRQSS,

    /// Sixel graphics passthrough
    sixel: Sixel,

    /// Tmux control mode
    tmux: if (build_options.tmux_control_mode)
        terminal.tmux.ControlNotification
    else
        void,

    pub fn deinit(self: *Command) void {
        switch (self.*) {
            .xtgettcap => |*v| v.data.deinit(),
            .decrqss => {},
            .sixel => |*v| v.data.deinit(),
            .tmux => {},
        }
    }

    pub const XTGETTCAP = struct {
        data: std.Io.Writer.Allocating,
        i: usize = 0,

        /// Returns the next terminfo key being requested and null
        /// when there are no more keys. The returned value is NOT hex-decoded
        /// because we expect to use a comptime lookup table.
        pub fn next(self: *XTGETTCAP) ?[]const u8 {
            const items = self.data.written();
            if (self.i >= items.len) return null;
            var rem = items[self.i..];
            const idx = std.mem.indexOf(u8, rem, ";") orelse rem.len;

            // Note that if we're at the end, idx + 1 is len + 1 so we're over
            // the end but that's okay because our check above is >= so we'll
            // never read.
            self.i += idx + 1;

            return rem[0..idx];
        }
    };

    /// Sixel graphics passthrough. The data is the complete re-synthesized
    /// DCS sequence (`ESC P <params> q <payload> ESC \`), suitable for
    /// writing verbatim to a sixel-capable terminal. The payload is NOT
    /// decoded or validated beyond DCS framing.
    pub const Sixel = struct {
        data: std.Io.Writer.Allocating,

        pub fn bytes(self: *Sixel) []const u8 {
            return self.data.written();
        }
    };

    /// Supported DECRQSS settings
    pub const DECRQSS = enum {
        none,
        sgr,
        decscusr,
        decstbm,
        decslrm,
    };
};

const State = union(enum) {
    /// We're not in a DCS state at the moment.
    inactive,

    /// We're hooked, but its an unknown DCS command or one that went
    /// invalid due to some bad input, so we're ignoring the rest.
    ignore,

    /// XTGETTCAP
    xtgettcap: std.Io.Writer.Allocating,

    /// Sixel graphics passthrough. Accumulates the full re-synthesized
    /// sequence bytes.
    sixel: std.Io.Writer.Allocating,

    /// DECRQSS
    decrqss: struct {
        data: [2]u8 = undefined,
        len: u2 = 0,
    },

    /// Tmux control mode: https://github.com/tmux/tmux/wiki/Control-Mode
    tmux: if (build_options.tmux_control_mode)
        terminal.tmux.ControlParser
    else
        void,

    pub fn deinit(self: *State) void {
        switch (self.*) {
            .inactive,
            .ignore,
            => {},

            .xtgettcap => |*v| v.deinit(),
            .sixel => |*v| v.deinit(),
            .decrqss => {},
            .tmux => |*v| if (comptime build_options.tmux_control_mode) {
                v.deinit();
            } else unreachable,
        }
    }
};

test "unknown DCS command" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .final = 'A' }) == null);
    try testing.expect(h.state == .ignore);
    try testing.expect(h.unhook() == null);
    try testing.expect(h.state == .inactive);
}

test "XTGETTCAP command" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .intermediates = "+", .final = 'q' }) == null);
    for ("536D756C78") |byte| _ = h.put(byte);
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .xtgettcap);
    try testing.expectEqualStrings("536D756C78", cmd.xtgettcap.next().?);
    try testing.expect(cmd.xtgettcap.next() == null);
}

test "XTGETTCAP mixed case" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .intermediates = "+", .final = 'q' }) == null);
    for ("536d756C78") |byte| _ = h.put(byte);
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .xtgettcap);
    try testing.expectEqualStrings("536D756C78", cmd.xtgettcap.next().?);
    try testing.expect(cmd.xtgettcap.next() == null);
}

test "XTGETTCAP command multiple keys" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .intermediates = "+", .final = 'q' }) == null);
    for ("536D756C78;536D756C78") |byte| _ = h.put(byte);
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .xtgettcap);
    try testing.expectEqualStrings("536D756C78", cmd.xtgettcap.next().?);
    try testing.expectEqualStrings("536D756C78", cmd.xtgettcap.next().?);
    try testing.expect(cmd.xtgettcap.next() == null);
}

test "XTGETTCAP command invalid data" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .intermediates = "+", .final = 'q' }) == null);
    for ("who;536D756C78") |byte| _ = h.put(byte);
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .xtgettcap);
    try testing.expectEqualStrings("WHO", cmd.xtgettcap.next().?);
    try testing.expectEqualStrings("536D756C78", cmd.xtgettcap.next().?);
    try testing.expect(cmd.xtgettcap.next() == null);
}

test "DECRQSS command" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .intermediates = "$", .final = 'q' }) == null);
    _ = h.put('m');
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .decrqss);
    try testing.expect(cmd.decrqss == .sgr);
}

test "DECRQSS invalid command" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .intermediates = "$", .final = 'q' }) == null);
    _ = h.put('z');
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .decrqss);
    try testing.expect(cmd.decrqss == .none);

    h.discard();

    try testing.expect(h.hook(alloc, .{ .intermediates = "$", .final = 'q' }) == null);
    _ = h.put('"');
    _ = h.put(' ');
    _ = h.put('q');
    try testing.expect(h.unhook() == null);
}

test "tmux enter and implicit exit" {
    if (comptime !build_options.tmux_control_mode) return error.SkipZigTest;

    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();

    {
        var cmd = h.hook(alloc, .{ .params = &.{1000}, .final = 'p' }).?;
        defer cmd.deinit();
        try testing.expect(cmd == .tmux);
        try testing.expect(cmd.tmux == .enter);
    }

    {
        var cmd = h.unhook().?;
        defer cmd.deinit();
        try testing.expect(cmd == .tmux);
        try testing.expect(cmd.tmux == .exit);
    }
}

test "sixel passthrough reconstructs full sequence" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .params = &.{ 0, 0, 8 }, .final = 'q' }) == null);
    for ("\"1;1;4;4#0;2;0;0;0#0!4~") |byte| _ = h.put(byte);
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .sixel);
    try testing.expectEqualStrings(
        "\x1bP0;0;8q\"1;1;4;4#0;2;0;0;0#0!4~\x1b\\",
        cmd.sixel.bytes(),
    );
}

test "sixel passthrough without params" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{};
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .final = 'q' }) == null);
    for ("#0~") |byte| _ = h.put(byte);
    var cmd = h.unhook().?;
    defer cmd.deinit();
    try testing.expect(cmd == .sixel);
    try testing.expectEqualStrings("\x1bPq#0~\x1b\\", cmd.sixel.bytes());
}

test "sixel passthrough over cap is dropped entirely" {
    const testing = std.testing;
    const alloc = testing.allocator;

    var h: Handler = .{ .max_bytes_sixel = 16 };
    defer h.deinit();
    try testing.expect(h.hook(alloc, .{ .params = &.{ 0, 0, 8 }, .final = 'q' }) == null);
    for ("#0;2;0;0;0#0!200~-!200~") |byte| _ = h.put(byte);
    try testing.expect(h.state == .ignore);
    try testing.expect(h.unhook() == null);
    try testing.expect(h.state == .inactive);
}
