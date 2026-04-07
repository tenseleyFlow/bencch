! CHECK: hello
! CHECK: goodbye world
! CHECK: hi
program test_string_deferred
    implicit none
    character(:), allocatable :: s

    s = 'hello'
    print *, s

    s = 'goodbye world'
    print *, s

    s = 'hi'
    print *, s
end program
