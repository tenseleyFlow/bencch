! CHECK: 6.28
program test_real_func
    implicit none
    real :: x, y
    x = 3.14
    y = double_it(x)
    print *, y
contains
    function double_it(val) result(r)
        real, intent(in) :: val
        real :: r
        r = val * 2.0
    end function
end program
