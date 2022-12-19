const std = @import("std");
const str = @import("str");
const builtin = @import("builtin");
const RocStr = str.RocStr;

const Align = extern struct { a: usize, b: usize };
extern fn malloc(size: usize) callconv(.C) ?*align(@alignOf(Align)) anyopaque;
extern fn realloc(c_ptr: [*]align(@alignOf(Align)) u8, size: usize) callconv(.C) ?*anyopaque;
extern fn free(c_ptr: [*]align(@alignOf(Align)) u8) callconv(.C) void;
extern fn memcpy(dest: *anyopaque, src: *anyopaque, count: usize) *anyopaque;

export fn roc_alloc(size: usize, alignment: u32) callconv(.C) ?*anyopaque {
    _ = alignment;

    return malloc(size);
}

export fn roc_realloc(c_ptr: *anyopaque, new_size: usize, old_size: usize, alignment: u32) callconv(.C) ?*anyopaque {
    _ = old_size;
    _ = alignment;

    return realloc(@alignCast(@alignOf(Align), @ptrCast([*]u8, c_ptr)), new_size);
}

export fn roc_dealloc(c_ptr: *anyopaque, alignment: u32) callconv(.C) void {
    _ = alignment;

    free(@alignCast(@alignOf(Align), @ptrCast([*]u8, c_ptr)));
}

export fn roc_memcpy(dest: *anyopaque, src: *anyopaque, count: usize) callconv(.C) void {
    _ = memcpy(dest, src, count);
}

export fn roc_panic(message: RocStr, tag_id: u32) callconv(.C) void {
    _ = tag_id;
    const msg = @ptrCast([*:0]const u8, c_ptr);
    const stderr = std.io.getStdErr().writer();
    stderr.print("Application crashed with message\n\n    {s}\n\nShutting down\n", .{msg}) catch unreachable;
    std.process.exit(0);
}

const FromHost = extern struct {
    eventHandlerId: usize,
    eventJsonList: RocStr, // it's really a list, but `roc build` gives us RocStr which is fine
    eventPlatformState: ?*anyopaque,
    initJson: RocStr, // it's really a list, but `roc build` gives us RocStr which is fine
    isInitEvent: bool,
};

const ToHost = extern struct {
    platformState: *anyopaque,
    eventPreventDefault: bool,
    eventStopPropagation: bool,
};

extern fn roc__main_1_exposed(FromHost) callconv(.C) ToHost;

var platformState: ?*anyopaque = null;

// Called from JS
export fn roc_vdom_init(init_pointer: ?[*]u8, init_length: usize, init_capacity: usize) callconv(.C) void {
    // it's really a list, but `roc build` gives us RocStr which is fine
    const init_json = RocStr{
        .str_bytes = init_pointer,
        .str_len = init_length,
        .str_capacity = init_capacity,
    };
    const from_host = FromHost{
        .eventHandlerId = 0,
        .eventJsonList = RocStr.empty(),
        .eventPlatformState = platformState,
        .initJson = init_json,
        .isInitEvent = true,
    };
    const to_host = roc__main_1_exposed(from_host);
    platformState = to_host.platformState;
}

// Called from JS
export fn roc_dispatch_event(list_ptr: ?[*]u8, list_length: usize, handler_id: usize) usize {
    // it's really a list, but `roc build` gives us RocStr which is fine
    const json_list = RocStr{
        .str_bytes = list_ptr,
        .str_len = list_length,
        .str_capacity = list_length,
    };
    const from_host = FromHost{
        .eventHandlerId = handler_id,
        .eventJsonList = json_list,
        .eventPlatformState = platformState,
        .initJson = RocStr.empty(),
        .isInitEvent = false,
    };
    const to_host = roc__main_1_exposed(from_host);
    platformState = to_host.platformState;
    return to_host.eventPreventDefault << 1 | to_host.eventStopPropagation;
}
