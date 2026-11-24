//! Micro-benchmarks for `neyuki bench`: the front end, the VM and the libraries.

use crate::bytecode::Proto;
use crate::compiler::{compile_source, try_compile_to_proto};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::vm::machine::VM;
use std::time::{Duration, Instant};

pub struct BenchResult {
    pub name: &'static str,
    pub iterations: usize,
    pub total_duration: Duration,
    pub avg_per_op: Duration,
    pub ops_per_sec: f64,
}

impl BenchResult {
    pub fn display(&self) {
        let avg_us = self.avg_per_op.as_nanos() as f64 / 1_000.0;
        let total_ms = self.total_duration.as_millis();
        println!(
            "{:<32} | {:>8} iters | {:>8.2} ms | {:>10.2} us/op | {:>12.0} ops/s",
            self.name, self.iterations, total_ms as f64, avg_us, self.ops_per_sec
        );
    }
}

fn measure<F: FnMut()>(name: &'static str, iterations: usize, mut f: F) -> BenchResult {
    if iterations == 0 {
        return BenchResult {
            name,
            iterations: 0,
            total_duration: Duration::ZERO,
            avg_per_op: Duration::ZERO,
            ops_per_sec: 0.0,
        };
    }

    // Warmup
    f();

    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    let total_duration = start.elapsed();
    let avg_per_op = total_duration / (iterations as u32);
    let total_secs = total_duration.as_secs_f64();
    let ops_per_sec = if total_secs > 0.0 {
        iterations as f64 / total_secs
    } else {
        0.0
    };

    BenchResult {
        name,
        iterations,
        total_duration,
        avg_per_op,
        ops_per_sec,
    }
}

fn prepare_proto(code: &str) -> Result<Proto, String> {
    let stmts = compile_source(code)?;
    try_compile_to_proto(&stmts)
}

fn bench_vm_script(name: &'static str, iterations: usize, code: &str) -> BenchResult {
    let proto = match prepare_proto(code) {
        Ok(proto) => proto,
        Err(err) => {
            eprintln!("Benchmark '{name}' compile error: {err}");
            return BenchResult {
                name,
                iterations: 0,
                total_duration: Duration::ZERO,
                avg_per_op: Duration::ZERO,
                ops_per_sec: 0.0,
            };
        }
    };
    measure(name, iterations, || {
        let mut vm = VM::new();
        if let Err(err) = vm.execute(proto.clone()) {
            eprintln!("Benchmark '{name}' runtime error: {err}");
        }
    })
}

fn run_vm_code(code: &str) {
    if let Ok(proto) = prepare_proto(code) {
        let mut vm = VM::new();
        let _ = vm.execute(proto);
    }
}

// 1. Lexer Benchmark
pub fn bench_lexer() -> BenchResult {
    let source = r#"
        local sum = 0
        for i = 1, 100 do
            local x = i * 2 + 1
            if x % 3 == 0 then
                sum = sum + x
            else
                sum = sum - (x >> 1)
            end
        end
        return sum
    "#;
    measure("1. Lexer Tokenization", 2_000, || {
        let mut lexer = Lexer::new(source);
        let _ = lexer.tokenize();
    })
}

// 2. Parser Benchmark
pub fn bench_parser() -> BenchResult {
    let source = r#"
        function compute(a, b, c)
            local res = {}
            for i = 1, 50 do
                res[i] = (a + b) * c - i
            end
            return res
        end
        return compute(10, 20, 30)
    "#;
    measure("2. AST Parser", 1_000, || {
        let mut parser = Parser::new(source);
        let _ = parser.parse_program();
    })
}

// 3. Compiler & Constant Folding Benchmark
pub fn bench_compiler() -> BenchResult {
    let source = r#"
        local a = 10 * 20 + 30 - 5
        local b = 1 << 4 | 2
        local c = math.sqrt(144) + math.floor(3.14)
        local function calc(x)
            return x * 2 + a + b + c
        end
        return calc(100)
    "#;
    let stmts = match compile_source(source) {
        Ok(stmts) => stmts,
        Err(err) => {
            eprintln!("Benchmark '3. Bytecode Compiler & Folding' AST error: {err}");
            return BenchResult {
                name: "3. Bytecode Compiler & Folding",
                iterations: 0,
                total_duration: Duration::ZERO,
                avg_per_op: Duration::ZERO,
                ops_per_sec: 0.0,
            };
        }
    };
    measure("3. Bytecode Compiler & Folding", 1_000, || {
        let _ = try_compile_to_proto(&stmts);
    })
}

// 4. VM Arithmetic & Loop Benchmark
pub fn bench_arithmetic() -> BenchResult {
    let code = r#"
        local sum = 0
        local i = 0
        while i < 10000 do
            sum = sum + (i * 3 - (i >> 1))
            i = i + 1
        end
        return sum
    "#;
    bench_vm_script("4. VM Arithmetic & Loop", 100, code)
}

// 5. VM Fibonacci Benchmark (Iterative)
pub fn bench_fibonacci() -> BenchResult {
    let code = r#"
        local a = 0
        local b = 1
        local i = 0
        while i < 1000 do
            local next = a + b
            a = b
            b = next
            i = i + 1
        end
        return b
    "#;
    bench_vm_script("5. VM Fibonacci (Iterative)", 200, code)
}

// 6. VM Table Benchmark (Creation, Hash Lookups, Array Insertions)
pub fn bench_tables() -> BenchResult {
    let code = r#"
        local t = {}
        for i = 1, 500 do
            t[i] = i * 2
            t["key_" .. tostring(i)] = i * 3
        end
        local acc = 0
        for i = 1, 500 do
            acc = acc + t[i] + t["key_" .. tostring(i)]
        end
        return acc
    "#;
    bench_vm_script("6. VM Table Operations", 50, code)
}

// 7. VM String Operations Benchmark (Concatenation, Sub, Formatting)
pub fn bench_strings() -> BenchResult {
    let code = r#"
        local string = require("@neyuki/string")
        local s = "hello, world, neyuki programming language!"
        local out = ""
        for i = 1, 200 do
            local sub = string.sub(s, 1, 12)
            out = string.format("%s-%d", sub, i)
        end
        return out
    "#;
    bench_vm_script("7. VM String Lib Operations", 50, code)
}

// 8. VM Coroutine Benchmark (Create, Yield, Resume throughput)
pub fn bench_coroutines() -> BenchResult {
    let code = r#"
        local co = require("@neyuki/coroutine")
        local gen = co.create(function()
            local x = 0
            while true do
                x = x + 1
                co.yield(x)
            end
        end)
        local sum = 0
        for i = 1, 1000 do
            local ok, val = co.resume(gen)
            sum = sum + val
        end
        return sum
    "#;
    bench_vm_script("8. VM Coroutines (1000 cycles)", 50, code)
}

// 9. VM Garbage Collection Benchmark (Cyclic reference collection)
pub fn bench_gc() -> BenchResult {
    let code = r#"
        for i = 1, 100 do
            local a = {}
            local b = {}
            a.b = b
            b.a = a
        end
        collectgarbage("collect")
    "#;
    bench_vm_script("9. VM Cyclic Garbage Collector", 50, code)
}

// 10. Crypto SHA-256 Hashing Benchmark
pub fn bench_crypto() -> BenchResult {
    let code = r#"
        local crypto = require("@neyuki/crypto")
        local data = "Neyuki high performance cryptographic hashing benchmark payload string 1234567890."
        for i = 1, 100 do
            local h = crypto.hash(data, "sha256")
        end
    "#;
    bench_vm_script("10. VM Crypto SHA-256 (100x)", 50, code)
}

// 11. Buffer Operations Benchmark (Raw memory read/write & copy)
pub fn bench_buffer() -> BenchResult {
    let code = r#"
        local buffer = require("@neyuki/buffer")
        local b1 = buffer.create(1024)
        local b2 = buffer.create(1024)
        for i = 0, 255 do
            buffer.writeu32(b1, i * 4, i * 100)
        end
        for i = 1, 50 do
            buffer.copy(b2, 0, b1, 0, 1024)
        end
        return buffer.readu32(b2, 0)
    "#;
    bench_vm_script("11. VM Buffer Raw Memory Ops", 100, code)
}

// 12. JSON Serialization & Deserialization Benchmark
pub fn bench_json() -> BenchResult {
    let code = r#"
        local json = require("@neyuki/json")
        local record = {
            id = 12345,
            name = "benchmark_test_object",
            active = true,
            scores = {10, 20, 30, 40, 50},
            meta = {
                created_at = "2026-09-17",
                version = "1.0.0"
            }
        }
        for i = 1, 50 do
            local encoded = json.encode(record)
            local decoded = json.decode(encoded)
        end
    "#;
    bench_vm_script("12. VM JSON Encode/Decode", 50, code)
}

// 13. VM End-to-End Pipeline Benchmark (Lex, Parse, Compile & Execute)
pub fn bench_end_to_end() -> BenchResult {
    let code = r#"
        local sum = 0
        for i = 1, 100 do
            sum = sum + i
        end
        return sum
    "#;
    measure("13. VM End-to-End Pipeline", 200, || {
        run_vm_code(code);
    })
}

// 14. VM Recursive Fibonacci Benchmark (Call stack frames, function dispatch, tree recursion)
pub fn bench_fibonacci_recursive() -> BenchResult {
    let code = r#"
        const function fibonacci(n: int): int
            if n <= 1 then
                return n
            end
            return fibonacci(n - 1) + fibonacci(n - 2)
        end
        return fibonacci(18)
    "#;
    bench_vm_script("14. VM Recursive Fibonacci", 50, code)
}

// 15. VM BigInt Arithmetic & Widening Benchmark (Multi-limb ops, dynamic widening, pow, shifts, div/mod)
pub fn bench_bigint() -> BenchResult {
    let code = r#"
        local base = 2^100
        local factor = 3^60
        local sum = 0
        for i = 1, 1000 do
            local widened = (1 << (60 + (i % 6))) * (i + 1)
            local product = (base + widened) * (factor - i)
            local quotient = product // (base >> 20)
            sum = sum + (quotient % 1000000007)
            base = base + (quotient >> 15)
        end
        return sum
    "#;
    bench_vm_script("15. VM BigInt Operations", 50, code)
}

// 16. VM Table Lookup Throughput Benchmark (Balanced array indexing, named fields, dynamic keys, nested accesses)
pub fn bench_table_lookup() -> BenchResult {
    let code = r#"
        local arr = {}
        for i = 1, 1000 do
            arr[i] = i * 7
        end

        local obj = {
            id = 1001,
            name = 2002,
            score = 3003,
            status = 4004,
            count = 5005,
            weight = 6006,
            tag = 7007,
            level = 8008,
            role = 9009,
            priority = 1010,
            nested = {
                a = 11,
                b = 22,
                c = 33,
                d = 44,
                inner = { x = 100, y = 200 }
            }
        }

        local keys = { "id", "name", "score", "status", "count", "weight", "tag", "level", "role", "priority" }

        local sum = 0
        for _ = 1, 200 do
            for j = 1, 10 do
                sum = sum + arr[j * 99]
            end
            sum = sum + obj.id + obj.name + obj.score + obj.status + obj.count
            sum = sum + obj.weight + obj.tag + obj.level + obj.role + obj.priority
            for _, k in keys do
                sum = sum + obj[k]
            end
            sum = sum + obj.nested.a + obj.nested.b + obj.nested.c + obj.nested.d
            sum = sum + obj.nested.inner.x + obj.nested.inner.y
        end
        return sum
    "#;
    bench_vm_script("16. VM Table Lookup Throughput", 50, code)
}

// 17. VM GC Allocation Pressure Benchmark (Nursery churn, live retention, threshold triggers, cycle sweep)
pub fn bench_gc_pressure() -> BenchResult {
    let code = r#"
        local keep = {}
        for i = 1, 15000 do
            local obj = {
                id = i,
                data = { val = i * 3, extra = i + 10 },
                tags = { i, i * 2, i * 3 }
            }
            if i % 4 == 0 then
                keep[#keep + 1] = obj
            end
        end

        local sum = 0
        for _, item in keep do
            sum = sum + item.id + item.data.val + item.tags[2]
        end

        collectgarbage("collect")
        return sum
    "#;
    bench_vm_script("17. VM GC Allocation Pressure", 30, code)
}

// Run the full benchmark suite and display formatted results
pub fn run_all_benchmarks() {
    println!(
        "========================================================================================="
    );
    println!(
        "                               NEYUKI BENCHMARK SUITE                                    "
    );
    println!(
        "========================================================================================="
    );
    println!(
        "{:<32} | {:>14} | {:>11} | {:>16} | {:>12}",
        "Benchmark Name", "Iterations", "Total Time", "Latency (avg)", "Throughput"
    );
    println!(
        "---------------------------------+----------------+-------------+------------------+-------------"
    );

    let results = [
        bench_lexer(),
        bench_parser(),
        bench_compiler(),
        bench_arithmetic(),
        bench_fibonacci(),
        bench_tables(),
        bench_strings(),
        bench_coroutines(),
        bench_gc(),
        bench_crypto(),
        bench_buffer(),
        bench_json(),
        bench_end_to_end(),
        bench_fibonacci_recursive(),
        bench_bigint(),
        bench_table_lookup(),
        bench_gc_pressure(),
    ];

    for res in &results {
        res.display();
    }

    println!(
        "========================================================================================="
    );
    println!("All benchmarks completed successfully.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::value::Value;

    #[test]
    fn test_benchmarks_smoke() {
        // Ensure all benchmark functions execute cleanly without panic
        assert!(bench_lexer().ops_per_sec > 0.0);
        assert!(bench_parser().ops_per_sec > 0.0);
        assert!(bench_compiler().ops_per_sec > 0.0);
        assert!(bench_arithmetic().ops_per_sec > 0.0);
        assert!(bench_fibonacci().ops_per_sec > 0.0);
        assert!(bench_tables().ops_per_sec > 0.0);
        assert!(bench_strings().ops_per_sec > 0.0);
        assert!(bench_coroutines().ops_per_sec > 0.0);
        assert!(bench_gc().ops_per_sec > 0.0);
        assert!(bench_crypto().ops_per_sec > 0.0);
        assert!(bench_buffer().ops_per_sec > 0.0);
        assert!(bench_json().ops_per_sec > 0.0);
        assert!(bench_end_to_end().ops_per_sec > 0.0);
        assert!(bench_fibonacci_recursive().ops_per_sec > 0.0);
        assert!(bench_bigint().ops_per_sec > 0.0);
        assert!(bench_table_lookup().ops_per_sec > 0.0);
        assert!(bench_gc_pressure().ops_per_sec > 0.0);
    }

    #[test]
    fn test_new_benchmarks_correctness() -> Result<(), String> {
        let run = |code: &str| -> Result<Value, String> {
            let proto = prepare_proto(code)?;
            let mut vm = VM::new();
            vm.execute(proto)
        };

        // 14. Recursive Fibonacci: fib(18) == 2584
        let fib_code = r#"
            const function fibonacci(n: int): int
                if n <= 1 then
                    return n
                end
                return fibonacci(n - 1) + fibonacci(n - 2)
            end
            return fibonacci(18)
        "#;
        assert_eq!(run(fib_code)?, Value::int(2584));

        // 15. BigInt: dynamic widening and multi-limb arithmetic
        let bigint_code = r#"
            local base = 2^100
            local factor = 3^60
            local sum = 0
            for i = 1, 10 do
                local widened = (1 << (60 + (i % 6))) * (i + 1)
                local product = (base + widened) * (factor - i)
                local quotient = product // (base >> 20)
                sum = sum + (quotient % 1000000007)
                base = base + (quotient >> 15)
            end
            return sum
        "#;
        let bigint_res = run(bigint_code)?;
        if let Some(v) = bigint_res.as_int() {
            assert!(v > 0);
        } else if !bigint_res.is_bigint() {
            return Err(format!("expected int or bigint, got {:?}", bigint_res));
        }

        // 16. Table lookup throughput: array, named fields, dynamic keys, nested accesses
        let table_code = r#"
            local arr = {}
            for i = 1, 100 do arr[i] = i * 7 end
            local obj = {
                id = 1001, name = 2002, score = 3003, status = 4004, count = 5005,
                weight = 6006, tag = 7007, level = 8008, role = 9009, priority = 1010,
                nested = { a = 11, b = 22, c = 33, d = 44, inner = { x = 100, y = 200 } }
            }
            local keys = { "id", "name", "score", "status", "count", "weight", "tag", "level", "role", "priority" }
            local sum = 0
            for _ = 1, 5 do
                for j = 1, 10 do sum = sum + arr[j * 9] end
                sum = sum + obj.id + obj.name + obj.score + obj.status + obj.count
                sum = sum + obj.weight + obj.tag + obj.level + obj.role + obj.priority
                for _, k in keys do sum = sum + obj[k] end
                sum = sum + obj.nested.a + obj.nested.b + obj.nested.c + obj.nested.d
                sum = sum + obj.nested.inner.x + obj.nested.inner.y
            end
            return sum
        "#;
        let table_res = run(table_code)?;
        if let Some(v) = table_res.as_int() {
            assert!(v > 0);
        } else {
            return Err(format!("expected int, got {:?}", table_res));
        }

        // 17. GC allocation pressure: live retention and cycle sweep
        let gc_code = r#"
            local keep = {}
            for i = 1, 100 do
                local obj = {
                    id = i,
                    data = { val = i * 3, extra = i + 10 },
                    tags = { i, i * 2, i * 3 }
                }
                if i % 4 == 0 then
                    keep[#keep + 1] = obj
                end
            end
            local sum = 0
            for _, item in keep do
                sum = sum + item.id + item.data.val + item.tags[2]
            end
            collectgarbage("collect")
            return sum
        "#;
        let gc_res = run(gc_code)?;
        if let Some(v) = gc_res.as_int() {
            assert!(v > 0);
        } else {
            return Err(format!("expected int, got {:?}", gc_res));
        }

        Ok(())
    }
}
