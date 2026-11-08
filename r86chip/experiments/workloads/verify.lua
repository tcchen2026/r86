-- Deterministic user-mode workload, run by the actual i386 Lua executable.
local sum = 0
for i = 1, 1000 do sum = sum + i end
assert(sum == 500500)
local function fib(n)
    if n < 2 then return n end
    return fib(n - 1) + fib(n - 2)
end
assert(fib(16) == 987)
local primes = {}
for n = 2, 1000 do
    local prime = true
    for _, p in ipairs(primes) do
        if p * p > n then break end
        if n % p == 0 then prime = false; break end
    end
    if prime then primes[#primes + 1] = n end
end
assert(#primes == 168 and primes[#primes] == 997)
assert(math.abs(math.sqrt(2) ^ 2 - 2) < 1e-12)
local words = {"gamma", "alpha", "beta"}
table.sort(words)
assert(table.concat(words, ":") == "alpha:beta:gamma")
local ok, message = pcall(function() error("expected-error") end)
assert(not ok and message:match("expected%-error"))
local co = coroutine.create(function()
    for i = 1, 5 do coroutine.yield(i * i) end
end)
for i = 1, 5 do
    local resumed, value = coroutine.resume(co)
    assert(resumed and value == i * i)
end
local bytes = {}
for i = 0, 255 do bytes[#bytes + 1] = string.char(i) end
local contents = table.concat(bytes)
local path = "/tmp/r86chip-lua-data"
local f = assert(io.open(path, "wb"))
assert(f:write(contents)); assert(f:close())
f = assert(io.open(path, "rb"))
assert(f:read("*a") == contents); assert(f:close())
assert(os.remove(path))
assert(os.execute("printf shell-and-lua >/tmp/r86chip-shell-data"))
collectgarbage("collect")
print(_VERSION)
print("R86_LUA_RESULTS sum=500500 fib=987 primes=168 last=997 io=256 coroutine=5")
