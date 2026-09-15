package main

import (
	"strconv"
	"sync"
)

func main() {
	var wg sync.WaitGroup

	for i := range 10 {
		wg.Go(func() {
			println("Hello, Go! " + strconv.Itoa(i))
		})
	}

	wg.Wait()
}
