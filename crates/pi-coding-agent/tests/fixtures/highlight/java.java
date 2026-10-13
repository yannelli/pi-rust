package com.example;

import java.util.List;

/** A greeter. */
public class Greeter<T> extends Base implements Runnable {
    private static final int COUNT = 42;

    @Override
    public void run() {
        // Print greetings
        for (int i = 0; i < COUNT; i++) {
            System.out.println("Hello " + i + 'c');
        }
        List<String> names = null;
        boolean ok = true;
    }
}
