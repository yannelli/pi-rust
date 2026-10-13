#include <iostream>
#include "local.h"

namespace demo {

template <typename T>
class Box {
public:
    explicit Box(T value) : value_(value) {}
    T get() const { return value_; }
private:
    T value_;
};

}  // namespace demo

int main(int argc, char** argv) {
    auto box = demo::Box<int>(42);
    std::cout << "value: " << box.get() << std::endl;
    return nullptr == argv ? 1 : 0;
}
