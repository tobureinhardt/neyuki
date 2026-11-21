# @neyuki/test

Writing tests: `describe` blocks, `it` cases, hooks, expectations whose failures show a diff, snapshots and a summary line. Tests run once the main chunk returns, so a test file is only declarations; `neyuki test` runs every `.nyk` file in `tests/` and fails the ones whose tests fail.

```lua
local test = require("@neyuki/test")
local describe, it, expect = test.describe, test.it, test.expect

describe("stack", function()
    local stack
    test.beforeEach(function() stack = {} end)

    it("pushes", function()
        table.insert(stack, 1)
        expect(stack):toEqual({ 1 })
    end)

    it("starts empty", function()
        expect(#stack):toBe(0)
    end)
end)
```

```text
  ok    stack > pushes (0ms)
  ok    stack > starts empty (0ms)

test result: ok. 2 passed; 0 failed; finished in 0.00s
```

A failing test is listed after the run with its message, the file and line of the assertion, and for `toEqual`, `test.equal` and snapshots a diff of the two values and the path of the first difference:

```text
  FAIL  config > merges

failures:

  config > merges
    expected { port = 81, verbose = true } to equal { port = 80, verbose = true }
    at tests/config.nyk:8
      first difference at .port
      - expected
      + actual

      - { port = 80, verbose = true }
      + { port = 81, verbose = true }

test result: FAILED. 0 passed; 1 failed; finished in 0.00s
```

Then the run raises the error `1 of 1 tests failed`, so the script exits with status 1.

## test.describe(name: string, body: function, options: table?): ()

Groups the tests and hooks `body` declares under `name`; groups nest, and a test's full name joins them with ` > `. `options.skip` skips every test in the group and `options.only` runs only the marked groups and tests.

## test.it(name: string, fn: function, options: table?): ()

Declares a test. It passes when `fn` returns and fails when it raises an error, from an expectation or anything else. `options.skip` and `options.only` work as for `describe`.

## test.skip(name: string, fn: function?): ()

Declares a test that is listed as skipped and not run.

## test.only(name: string, fn: function): ()

Declares a test, and while any test or group is marked `only`, runs only those. The others count as filtered out.

## test.todo(name: string): ()

Lists a test still to write.

## test.beforeAll(fn: function): ()

Runs `fn` once before the tests of the enclosing `describe` (or of the file, outside one). If it fails, every test in the group fails with its message.

## test.afterAll(fn: function): ()

Runs `fn` once after the tests of the enclosing group, even when they failed.

## test.beforeEach(fn: function): ()

Runs `fn` before each test in the enclosing group and the groups inside it, outer groups' hooks first. If it fails, the test fails without running.

## test.afterEach(fn: function): ()

Runs `fn` after each test in the enclosing group and the groups inside it, inner groups' hooks first, even when the test failed.

## test.expect(value: any): Expectation

Wraps a value for the checks below. `expect(value).never` checks the opposite: `expect(x).never:toBeNil()`.

## Expectation:toBe(expected: any): ()

The value is `expected` itself: equal numbers, strings and booleans, or the very same table or function.

## Expectation:toEqual(expected: any): ()

The value has the same content as `expected`: tables are compared key by key, recursively (metatables are ignored, and cycles are allowed). Fails with a diff.

```lua
expect({ name = "Ada", langs = { "en" } }):toEqual({ name = "Ada", langs = { "en" } })
```

## Expectation:toBeNil(): ()

## Expectation:toBeTruthy(): ()

The value is neither `nil` nor `false`.

## Expectation:toBeFalsy(): ()

The value is `nil` or `false`.

## Expectation:toBeType(name: string): ()

`type(value)` is `name`.

## Expectation:toBeCloseTo(expected: number, tolerance: number?): ()

The number is within `tolerance` (default `1e-9`) of `expected`.

## Expectation:toBeGreaterThan(limit: number): ()

## Expectation:toBeGreaterThanOrEqual(limit: number): ()

## Expectation:toBeLessThan(limit: number): ()

## Expectation:toBeLessThanOrEqual(limit: number): ()

## Expectation:toContain(item: any): ()

A string contains `item` as plain text, or a table has an element equal to `item` (as `toEqual` compares).

## Expectation:toHaveLength(length: number): ()

`#value` is `length`, for a string or a table.

## Expectation:toMatch(pattern: string): ()

A string matches `pattern` (as `string.find` does).

## Expectation:toThrow(expected: string?): ()

The value is a function that raises an error when called, with `expected` in its message as plain text when given.

```lua
expect(function() int("12x") end):toThrow("12x")
```

## Expectation:toMatchSnapshot(name: string?): ()

The value matches the snapshot stored for this test. Snapshots live next to the test file, in `__snapshots__/<file>.snap`, keyed by the test's full name and a counter (or `name`). A snapshot that does not exist yet is written and the run reports it; one that differs fails with a diff. Run with the environment variable `NEYUKI_UPDATE_SNAPSHOTS=1` to replace those instead. Strings are stored as written and other values as `test.inspect` formats them. Commit the `.snap` files with the tests.

## test.equal(actual: any, expected: any, message: string?): ()

Fails unless `actual` has the same content as `expected`, as `toEqual` does, with `message` instead of the default one.

## test.fail(message: string?): ()

Fails the running test.

## test.equals(a: any, b: any): boolean

Whether two values have the same content, compared as `toEqual` does.

## test.inspect(value: any): string

Formats a value as stable text: strings quoted, numbers and booleans as `tostring` does, table keys in order (the array part first, then the other keys sorted), short flat tables on one line and the others one entry per line, `<cycle>` for a table inside itself and `<function>`, `<thread>` or `<buffer>` for other values.

```lua
print(test.inspect({ 3, z = true, a = "x" })) --> { 3, a = "x", z = true }
```

## test.diff(expected: any, actual: any): string?

The diff `toEqual` shows for two values, or `nil` when they are equal.

## test.run(options: table?): table

Runs the tests declared so far, prints a line per test, the failures and the summary line, and writes the snapshots that changed. Declared tests run by themselves after the main chunk, so call it only to run them earlier or with options; it raises `N of M tests failed` when a test failed. Options:

- `filter`: run only the tests whose full name contains this text
- `updateSnapshots`: replace snapshots that differ (default: whether `NEYUKI_UPDATE_SNAPSHOTS` is set to something other than `0`)
- `output`: a function called with each line instead of `print`
- `raise`: `false` to return without raising when a test failed

It returns `{ passed, failed, skipped, todo, filtered, failures, tests, snapshots }`: the counts, the failures as `{ name, message, where?, detail? }`, every test as `{ name, status }` (`"ok"`, `"FAIL"`, `"skip"` or `"todo"`), and `snapshots` as `{ written, updated, mismatched }`.

## test.new(): table

A separate suite with the same functions (`describe`, `it`, hooks, `expect`, `equal`, `run`, ...). It runs only when its `run` is called, so a package can run its own checks and read the results, and `suite.snapshotDir` sets a directory for its snapshot files.
