#include <boost/asio.hpp>
#include <boost/beast.hpp>
#include "telemetry.h"
#include <iostream>
#include <memory>

namespace net = boost::asio;
namespace beast = boost::beast;
using tcp = net::ip::tcp;
using local = net::local::stream_protocol;

template<class Socket>
class Session : public std::enable_shared_from_this<Session<Socket>> {
    beast::websocket::stream<Socket> ws;
    beast::flat_buffer buffer;
    bool telemetry;
    TelemetryState state;
    std::array<char, 24> reply;
    using std::enable_shared_from_this<Session<Socket>>::shared_from_this;

    void read() {
        ws.async_read(buffer, [self = shared_from_this()](beast::error_code ec, std::size_t) {
            if (ec) return;
            auto data = self->buffer.data();
            auto output = net::const_buffer(data.data(), data.size());
            if (self->telemetry) {
                if (self->ws.got_text()) throw std::runtime_error("telemetry requires binary messages");
                auto message = std::string_view(static_cast<const char *>(data.data()), data.size());
                self->reply = self->state.acknowledge(message);
                output = net::const_buffer(self->reply.data(), self->reply.size());
                self->ws.binary(true);
            } else {
                self->ws.text(self->ws.got_text());
            }
            self->ws.async_write(output, [self](beast::error_code ec, std::size_t) {
                if (ec) return;
                self->buffer.consume(self->buffer.size());
                self->read();
            });
        });
    }

public:
    explicit Session(Socket socket, bool telemetry) : ws(std::move(socket)), telemetry(telemetry) {}
    void start() {
        if constexpr (std::is_same_v<Socket, tcp::socket>)
            ws.next_layer().set_option(tcp::no_delay(true));
        ws.auto_fragment(false);
        ws.read_message_max(16 * 1024 * 1024);
        ws.async_accept([self = shared_from_this()](beast::error_code ec) {
            if (!ec) self->read();
        });
    }
};

template<class Acceptor>
void accept(Acceptor &listener, bool telemetry) {
    listener.async_accept([&listener, telemetry](beast::error_code ec, auto socket) {
        if (!ec) std::make_shared<Session<decltype(socket)>>(std::move(socket), telemetry)->start();
        accept(listener, telemetry);
    });
}

int main(int argc, char **argv) {
    try {
        if (argc != 2 && argc != 3) throw std::runtime_error("expected bind address");
        bool telemetry = argc == 3 && std::string_view(argv[2]) == "telemetry";
        if (argc == 3 && !telemetry) throw std::runtime_error("expected telemetry workload");
        net::io_context io(1);
        if (std::string_view(argv[1]).starts_with("unix:")) {
            local::acceptor listener(io, local::endpoint(argv[1] + 5));
            std::cout << "READY unix" << std::endl;
            accept(listener, telemetry);
            io.run();
            return 0;
        }
        auto address = net::ip::make_address(argv[1]);
        if (address.is_unspecified()) throw std::runtime_error("explicit bind IP required");
        tcp::acceptor listener(io, {address, 0});
        std::cout << "READY " << listener.local_endpoint().port() << std::endl;
        accept(listener, telemetry);
        io.run();
    } catch (std::exception const &e) {
        std::cerr << e.what() << '\n';
        return 1;
    }
}
