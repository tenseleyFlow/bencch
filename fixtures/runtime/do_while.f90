! CHECK: 1
program test_do_while
    implicit none
    integer :: x
    x = 100
    do while (x > 1)
        x = x / 2
    end do
    print *, x
end program
