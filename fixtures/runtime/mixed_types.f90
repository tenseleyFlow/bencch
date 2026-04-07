! CHECK: 5.5
program mixed_types
    implicit none
    integer :: a
    real :: b, c
    a = 3
    b = 2.5
    c = a + b
    print *, c
end program
