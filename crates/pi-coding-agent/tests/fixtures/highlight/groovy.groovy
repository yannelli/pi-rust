package demo

import groovy.transform.CompileStatic

// A greeter
@CompileStatic
class Greeter extends Base {
    String name = 'world'
    int count = 42

    String greet(int times = 1) {
        def parts = []
        times.times { i -> parts << "Hello ${name} ${i}" }
        return parts ? parts.join(', ') : null
    }
}

println new Greeter().greet(2) + true
