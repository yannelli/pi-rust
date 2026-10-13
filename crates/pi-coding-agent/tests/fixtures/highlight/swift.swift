import Foundation

// A greeter
protocol Greeting {
    func greet(_ name: String) -> String
}

struct Greeter: Greeting {
    let count: Int = 42

    func greet(_ name: String) -> String {
        guard count > 0 else { return "" }
        return "Hello \(name)! \(count * 2)"
    }
}

let g = Greeter()
print(g.greet("world"), true, nil as Int?)
