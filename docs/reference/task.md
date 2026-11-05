# task

A scheduler for running several pieces of code side by side. Each task is a coroutine: it runs until it calls `task.wait`, and the scheduler resumes it once the wait is over, running other tasks in the meantime. The model follows Luau's `task` library.

```lua
task.spawn(function()
    while task.wait(1) do
        print("Hello World!")
    end
end)
print("im still running!")
```

The scheduler gets control whenever the main program calls `task.wait`, and when the main program finishes: the script then keeps running until no task is waiting any more. A task that raises an error prints `task error: <message>` to stderr while the others carry on, and the script exits with status 1 at the end.

Inside a task, `task.wait` suspends only that task. Where a task cannot yield (in the main program, or inside a function that a native called back, such as a `try` body), it blocks instead and runs the other tasks while it waits.

## task.spawn(f: function | thread, ...: any): thread

Runs `f` as a task right away, passing it the arguments, until it first waits or yields. Returns its thread.

## task.defer(f: function | thread, ...: any): thread

Like `spawn`, but `f` starts on the next scheduler pass instead of right away.

## task.delay(seconds: number, f: function | thread, ...: any): thread

Starts `f` with the arguments once `seconds` have passed.

## task.wait(seconds: number?): number

Suspends the current task for `seconds` (fractions allowed) and returns the seconds actually waited. With no argument it waits until the next scheduler pass.

## task.cancel(thread: thread)

Unschedules a task and closes its coroutine, so it never runs again. A running task cannot cancel itself.

## Blocking calls

Inside a task, calls that would block on the outside world (`http.request` and the helpers built on it, `os.run` and reading files with `fs.read`) suspend only that task. The work runs on a background thread, the other tasks keep running, and the task resumes with the result (or the error) once the work finishes. Under `ui.run` finished work is picked up every frame, so the window stays responsive while a request is in flight.

Where a task cannot yield (the main program, a plain coroutine or a function a native called back, such as a `try` body) these calls block as before. A task cancelled while its work runs never resumes; the work itself runs to its end, so give requests a `timeout`.

## Windows in a task

`ui.poll` with a timeout also suspends only the calling task, so a whole UI loop can run as a task:

```lua
local game = task.spawn(function()
    ui.run(function(events, dt) end)
end)
print(coroutine.status(game)) --> suspended, printed at once
task.wait(1) -- the window runs while the main program waits
```

While a task waits in `ui.poll`, the scheduler pumps the window system's events itself and hands them to that task, so input is handled as it arrives (not at the next `task.wait` deadline). The window runs whenever the main program calls `task.wait` and after the main program finishes; the script then exits once the window is closed.

Limits:

- Run one event loop at a time. Events go to whichever waiting task is first in line; the others get an empty list when their timeout ends.
- Scheduling is cooperative: a main program that computes for a long time without calling `task.wait` leaves the window unserved and frozen.
- `task.wait` inside the `ui.run` handler still blocks that loop, because the handler runs under `try` (a native), where a task cannot yield.
- `ui.poll` without a timeout, in the main program, in a plain coroutine or under `try` behaves as before: it pumps in place and does not yield.
