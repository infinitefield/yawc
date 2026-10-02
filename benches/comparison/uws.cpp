#include "App.h"
#include "telemetry.h"
#include <iostream>

int main(int argc, char **argv) {
    if ((argc != 2 && argc != 3) || std::string_view(argv[1]) == "::" || std::string_view(argv[1]) == "0.0.0.0") {
        std::cerr << "expected explicit bind address\n";
        return 1;
    }
    bool telemetry = argc == 3 && std::string_view(argv[2]) == "telemetry";
    if (argc == 3 && !telemetry) {
        std::cerr << "expected telemetry workload\n";
        return 1;
    }
    struct Data { TelemetryState state; };
    bool listening = false;
    auto app = uWS::App();
    app.ws<Data>("/*", {
        .compression = uWS::DISABLED,
        .maxPayloadLength = 16 * 1024 * 1024,
        .idleTimeout = 0,
        .maxBackpressure = 16 * 1024 * 1024,
        .closeOnBackpressureLimit = true,
        .sendPingsAutomatically = false,
        .message = [telemetry](auto *ws, std::string_view message, uWS::OpCode opcode) {
            if (telemetry) {
                if (opcode != uWS::OpCode::BINARY) {
                    std::cerr << "telemetry requires binary messages\n";
                    std::exit(1);
                }
                auto reply = ws->getUserData()->state.acknowledge(message);
                message = std::string_view(reply.data(), reply.size());
                opcode = uWS::OpCode::BINARY;
                if (ws->send(message, opcode, false) == uWS::WebSocket<false, true, Data>::DROPPED) {
                    std::cerr << "telemetry reply dropped\n";
                    std::exit(1);
                }
                return;
            }
            if (ws->send(message, opcode, false) == uWS::WebSocket<false, true, Data>::DROPPED) {
                std::cerr << "echo dropped\n";
                std::exit(1);
            }
        },
    });
    auto ready = [&](auto *socket) {
        if (socket) {
            listening = true;
            if (std::string_view(argv[1]).starts_with("unix:")) {
                std::cout << "READY unix" << std::endl;
            } else {
                std::cout << "READY " << us_socket_local_port(0, reinterpret_cast<us_socket_t *>(socket)) << std::endl;
            }
        }
    };
    if (std::string_view(argv[1]).starts_with("unix:")) {
        app.listen(ready, argv[1] + 5);
    } else {
        app.listen(argv[1], 0, ready);
    }
    app.run();
    return listening ? 0 : 1;
}
