package main

import (
	"fmt"
	"strings"
)

// Point is a 2D point.
type Point struct {
	X, Y int
}

func (p *Point) String() string {
	return fmt.Sprintf("(%d, %d)", p.X, p.Y)
}

func main() {
	p := &Point{X: 1, Y: 2}
	if p != nil && true {
		fmt.Println(strings.ToUpper(p.String()), 3.5, `raw`)
	}
}
