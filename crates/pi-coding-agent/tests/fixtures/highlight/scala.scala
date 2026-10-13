package demo

import scala.collection.mutable

// A greeter
case class Greeter(name: String) extends Base {
  val count: Int = 42

  def greet(times: Int = 1): String = {
    val parts = mutable.ListBuffer[String]()
    for (i <- 0 until times) parts += s"Hello $name $i"
    if (parts.isEmpty) null else parts.mkString(", ")
  }
}

object Main extends App {
  println(Greeter("world").greet(2) + true)
}
