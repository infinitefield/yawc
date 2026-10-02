#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <stdexcept>
#include <string_view>

struct TelemetryState {
    std::uint64_t messages = 0;
    std::uint64_t total = 0;

    static std::uint64_t read_le(std::string_view input, std::size_t offset, std::size_t size) {
        std::uint64_t value = 0;
        for (std::size_t i = 0; i < size; ++i)
            value |= std::uint64_t(static_cast<unsigned char>(input[offset + i])) << (8 * i);
        return value;
    }

    static void write_le(std::array<char, 24> &output, std::size_t offset, std::uint64_t value) {
        for (std::size_t i = 0; i < 8; ++i)
            output[offset + i] = static_cast<char>(value >> (8 * i));
    }

    std::array<char, 24> acknowledge(std::string_view input) {
        if (input.size() < 12 || (input.size() - 8) % 4 != 0)
            throw std::runtime_error("invalid telemetry batch");

        auto sequence = read_le(input, 0, 8);
        for (std::size_t offset = 8; offset < input.size(); offset += 4)
            total += read_le(input, offset, 4);
        ++messages;

        std::array<char, 24> reply;
        write_le(reply, 0, sequence);
        write_le(reply, 8, total);
        write_le(reply, 16, messages);
        return reply;
    }
};
