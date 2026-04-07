! CHECK: 3628800
program test_factorial
    implicit none
    integer :: n, result
    n = 10
    result = factorial(n)
    print *, result
contains
    recursive function factorial(n) result(f)
        integer, intent(in) :: n
        integer :: f
        if (n <= 1) then
            f = 1
        else
            f = n * factorial(n - 1)
        end if
    end function
end program
