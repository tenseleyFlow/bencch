! CHECK: 25
program test_exit_cycle
    implicit none
    integer :: i, s
    s = 0
    do i = 1, 100
        if (i > 10) exit
        if (mod(i, 2) == 0) cycle
        s = s + i
    end do
    print *, s
end program
