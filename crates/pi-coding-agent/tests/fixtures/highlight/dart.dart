import 'dart:math';

// A greeter
class Greeter extends Base {
  final String name;
  int count = 42;

  Greeter(this.name);

  @override
  String greet([int times = 1]) {
    final parts = <String>[];
    for (var i = 0; i < max(times, 1); i++) {
      parts.add('Hello $name ${i + 1}');
    }
    return parts.isEmpty ? null : parts.join(', ');
  }
}

void main() => print(Greeter('world').greet(2) + true.toString());
