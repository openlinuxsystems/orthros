// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// C++ on Radix: LLVM's libc++, libc++abi and libunwind (ports/llvm-runtimes)
// as a program uses them. Exceptions thrown through frames and threads,
// RTTI, static and thread-local objects, streams, containers and
// algorithms, threads with mutexes, condition variables, atomics and
// futures, clocks, std::filesystem, random numbers, std::format,
// std::regex, number conversions and message catalogs.

#include <algorithm>
#include <atomic>
#include <charconv>
#include <chrono>
#include <condition_variable>
#include <exception>
#include <filesystem>
#include <format>
#include <fstream>
#include <future>
#include <locale>
#include <map>
#include <mutex>
#include <numeric>
#include <random>
#include <regex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <typeinfo>
#include <unordered_map>
#include <vector>

#include "check.h"

namespace {

// Set by a global object's constructor, which runs before main.
int constructed = 0;

struct Global {
	Global() { constructed = 42; }
};
Global global;

// Counts the objects destroyed as an exception unwinds the stack.
int destroyed = 0;

struct Guard {
	~Guard() { destroyed++; }
};

[[noreturn]] void throw_deep(int depth) {
	Guard guard;
	if(depth == 0)
		throw std::runtime_error("deep");
	throw_deep(depth - 1);
}

struct Base {
	virtual ~Base() = default;
};
struct Derived : Base {
	int value = 7;
};

void exceptions() {
	try {
		throw_deep(10);
	} catch(const std::runtime_error &error) {
		CHECK(std::string(error.what()) == "deep");
	}
	CHECK(destroyed == 11);
	// Rethrown, caught by base class, and as anything.
	int caught = 0;
	try {
		try {
			throw std::out_of_range("range");
		} catch(...) {
			caught++;
			throw;
		}
	} catch(const std::logic_error &) {
		caught++;
	}
	CHECK(caught == 2);
	// From the library itself.
	try {
		(void)std::vector<int>(3).at(5);
		CHECK(!"at() threw");
	} catch(const std::out_of_range &) {
	}
	// Carried from one thread to another.
	std::exception_ptr carried;
	std::thread([&] {
		try {
			throw std::invalid_argument("thread");
		} catch(...) {
			carried = std::current_exception();
		}
	}).join();
	try {
		std::rethrow_exception(carried);
	} catch(const std::invalid_argument &error) {
		CHECK(std::string(error.what()) == "thread");
	}
	// A throw from a noexcept function would end the program, so only
	// that it is noexcept.
	CHECK(noexcept(std::declval<std::vector<int> &>().size()));
}

void types() {
	CHECK(constructed == 42);
	std::unique_ptr<Base> object = std::make_unique<Derived>();
	auto *derived = dynamic_cast<Derived *>(object.get());
	CHECK(derived && derived->value == 7);
	Base &dynamic = *object;
	CHECK(typeid(dynamic) == typeid(Derived));
	CHECK(std::string(typeid(int).name()) == "i");
	try {
		Base base;
		(void)dynamic_cast<Derived &>(base);
		CHECK(!"dynamic_cast threw");
	} catch(const std::bad_cast &) {
	}
}

// Destroyed with its thread, which counts it.
std::atomic<int> thread_objects{0};

struct PerThread {
	~PerThread() { thread_objects++; }
};
thread_local PerThread per_thread;

void threads() {
	std::mutex mutex;
	std::condition_variable ready;
	int stage = 0;
	std::atomic<long> sum{0};
	std::vector<std::thread> workers;
	for(int i = 0; i < 4; i++) {
		workers.emplace_back([&, i] {
			(void)&per_thread;
			{
				std::unique_lock lock(mutex);
				ready.wait(lock, [&] { return stage == 1; });
			}
			for(int n = 0; n < 1000; n++)
				sum += i;
		});
	}
	{
		std::lock_guard lock(mutex);
		stage = 1;
	}
	ready.notify_all();
	for(auto &worker : workers)
		worker.join();
	CHECK(sum == 6000);
	CHECK(thread_objects == 4);
	auto answer = std::async(std::launch::async, [] { return 6 * 7; });
	CHECK(answer.get() == 42);
	std::promise<std::string> promise;
	auto future = promise.get_future();
	std::thread([&] { promise.set_value("kept"); }).join();
	CHECK(future.get() == "kept");
	CHECK(std::thread::hardware_concurrency() >= 1);
}

void time() {
	auto start = std::chrono::steady_clock::now();
	std::this_thread::sleep_for(std::chrono::milliseconds(20));
	auto waited = std::chrono::steady_clock::now() - start;
	CHECK(waited >= std::chrono::milliseconds(20));
	CHECK(waited < std::chrono::seconds(5));
	auto now = std::chrono::system_clock::now();
	// After 2025.
	CHECK(std::chrono::duration_cast<std::chrono::seconds>(now.time_since_epoch()).count() >
			1735689600);
}

void text() {
	std::ostringstream out;
	out << "pi " << 3.14159 << ' ' << std::hex << 255 << ' ' << std::boolalpha << true;
	CHECK(out.str() == "pi 3.14159 ff true");
	std::istringstream in("12 3.5 word");
	int number;
	double real;
	std::string word;
	in >> number >> real >> word;
	CHECK(number == 12 && real == 3.5 && word == "word");
	CHECK(std::format("{:>8.3f}|{:<4}|{:#x}", 2.5, "ab", 255) == "   2.500|ab  |0xff");
	CHECK(std::to_string(-17) == "-17");
	char buffer[32];
	auto written = std::to_chars(buffer, buffer + sizeof(buffer), 0.1);
	CHECK(std::string(buffer, written.ptr) == "0.1");
	double parsed = 0;
	const char *digits = "6.02214076e23";
	auto read = std::from_chars(digits, digits + strlen(digits), parsed);
	CHECK(read.ec == std::errc() && parsed == 6.02214076e23);
	std::regex pattern(R"((\w+)@(\w+)\.org)");
	std::smatch match;
	std::string address = "duke@dukovo.org";
	CHECK(std::regex_match(address, match, pattern) && match[2] == "dukovo");
	CHECK(std::regex_replace(std::string("a1b22c333"), std::regex("[0-9]+"), "#") == "a#b#c#");
	// The C.UTF-8 locale, and a catalog that is not there.
	std::locale utf8("C.UTF-8");
	CHECK(std::isalpha(L'é', utf8));
	auto &messages = std::use_facet<std::messages<char>>(utf8);
	CHECK(messages.open("/no/such/catalog", utf8) < 0);
}

void containers() {
	std::vector<int> numbers(100);
	std::iota(numbers.begin(), numbers.end(), 1);
	CHECK(std::accumulate(numbers.begin(), numbers.end(), 0) == 5050);
	std::mt19937 engine(12345);
	std::shuffle(numbers.begin(), numbers.end(), engine);
	std::sort(numbers.begin(), numbers.end());
	CHECK(std::is_sorted(numbers.begin(), numbers.end()) && numbers.front() == 1);
	std::map<std::string, int> ordered{{"b", 2}, {"a", 1}, {"c", 3}};
	CHECK(ordered.begin()->first == "a");
	std::unordered_map<int, std::string> hashed;
	for(int i = 0; i < 1000; i++)
		hashed[i] = std::to_string(i);
	CHECK(hashed.size() == 1000 && hashed[999] == "999");
	std::random_device device;
	std::uniform_int_distribution<int> die(1, 6);
	int roll = die(device);
	CHECK(roll >= 1 && roll <= 6);
}

void files() {
	namespace fs = std::filesystem;
	fs::path dir = "/tmp/cxx";
	std::error_code error;
	fs::remove_all(dir, error);
	CHECK(fs::create_directories(dir / "a" / "b"));
	{
		std::ofstream file(dir / "a" / "text");
		file << "line one\nline two\n";
	}
	std::ifstream file(dir / "a" / "text");
	std::string line;
	std::vector<std::string> lines;
	while(std::getline(file, line))
		lines.push_back(line);
	CHECK(lines.size() == 2 && lines[1] == "line two");
	CHECK(fs::file_size(dir / "a" / "text") == 18);
	CHECK(fs::is_directory(dir / "a" / "b"));
	fs::copy_file(dir / "a" / "text", dir / "copy");
	CHECK(fs::exists(dir / "copy"));
	fs::rename(dir / "copy", dir / "moved");
	int count = 0;
	for(const auto &entry : fs::recursive_directory_iterator(dir)) {
		(void)entry;
		count++;
	}
	CHECK(count == 4);
	CHECK(fs::current_path() == fs::path("/"));
	CHECK(fs::canonical(dir / "a" / ".." / "moved") == dir / "moved");
	try {
		(void)fs::file_size(dir / "missing");
		CHECK(!"file_size threw");
	} catch(const fs::filesystem_error &failure) {
		CHECK(failure.code() == std::errc::no_such_file_or_directory);
	}
	CHECK(fs::remove_all(dir) == 5);
}

} // namespace

int main() {
	check_start("cxx");
	RUN(exceptions);
	RUN(types);
	RUN(threads);
	RUN(time);
	RUN(text);
	RUN(containers);
	RUN(files);
	return check_end();
}
