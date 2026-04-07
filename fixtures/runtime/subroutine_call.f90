! CHECK: 30
program test_sub
    implicit none
    integer :: a, b, c
    a = 10
    b = 20
    call add_nums(a, b, c)
    print *, c
contains
    subroutine add_nums(x, y, result)
        integer, intent(in) :: x, y
        integer, intent(out) :: result
        result = x + y
    end subroutine
end program
