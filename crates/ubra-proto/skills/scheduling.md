# Scheduling work with Ubra

When the person wants something run later, at a set time, or repeatedly ("every weekday at 9", "tomorrow morning", "in 2 hours", "each night"), use the `schedule_agent` tool. Never use your own cron, loop, or reminder tools (CronCreate, /loop, /schedule) or a sleeping shell: those die with this session and skip runs the Mac slept through. A Ubra schedule is kept by the Engine and survives this session closing and Ubra restarting.

## How a run works

- The first run opens a **new** top-level session of `kind` in `cwd` and sends `prompt`. That session cannot see this conversation.
- Later runs send `prompt` again into that same session (resuming it if the agent exited), so a repeating schedule is one ongoing conversation. If the person closed the tab, the next run opens a new one. With `worktree: true` every run opens its own session instead.
- Write the prompt so it stands on its own: the goal, the repository and any context it needs, what to produce, and where the result goes (open a PR, write a file, add to a note).
- For code changes pass `worktree: true` and `base: "origin/main"` so every run starts from a clean checkout.
- Give a short `name`; it titles the schedule and the tab its runs use.

## When it runs

Give exactly one of:

- `cron`: five fields in the Mac's local time. `0 9 * * 1-5` is weekdays at 09:00, `30 8 * * *` every day at 08:30, `0 * * * *` every hour.
- `in_minutes`: a one-off relative to now.
- `at_ms`: a one-off at an exact moment (epoch milliseconds).

## Missed runs

If the Mac was asleep or Ubra was closed at the due time, the newest missed run fires once as soon as Ubra is back, as long as it is within `catch_up_hours` (default 12; 0 never catches up). Older occurrences are recorded as missed. Nothing fires twice.

## Waking the Mac

- `wake_mac: true` when the person wants the run to happen even if the Mac is asleep. Ubra wakes it 2 minutes early, keeps it awake while the agent works, then puts it back to sleep if nobody used it and no other session is busy.
- The lid must be open. A laptop with its lid closed, or a Mac that is off, cannot be woken; catch-up covers those cases.
- It needs a one-time approval: Settings > Schedules > "Allow ubra to wake the Mac". If `list_schedules` reports `wakeHelperError`, tell the person to turn it on.
- `keep_awake: true` only stops an already awake Mac from idle-sleeping around a run.

## After creating

Confirm the schedule back in plain words from the returned `nextDue`, for example: "Weekdays at 09:00, next run Monday at 09:00. It will wake the Mac."

## Checking and removing

- `list_schedules` shows each schedule's next due time and recent runs (`onTime`, `late` with `lateReason` asleep or notRunning, `missed`, `failed`, `manual`) and the session each run opened.
- `delete_schedule` removes a schedule. Sessions its runs opened stay.

## Limits

Ubra only starts the agent. Access to mail, calendars, or other services must come from that agent's own tools or connectors; say so if the task needs them.
