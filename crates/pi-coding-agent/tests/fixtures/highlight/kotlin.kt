package demo

import kotlin.math.max

// A greeter
data class Greeter(val name: String) : Base() {
    private val count = 42

    fun greet(times: Int = 1): String {
        val parts = mutableListOf<String>()
        for (i in 0 until max(times, 1)) parts += "Hello $name ${i + 1}"
        return if (parts.isEmpty()) "" else parts.joinToString(", ")
    }
}

fun main() = println(Greeter("world").greet(2) + null + true)
