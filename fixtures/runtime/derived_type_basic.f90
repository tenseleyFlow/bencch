! Basic derived type test: define type, declare variable, assign components, read back.
!
! CHECK: 42
! CHECK: 3.14
program test_derived_type
    implicit none

    type :: point
        integer :: id
        real :: x
    end type

    type(point) :: p

    p%id = 42
    p%x = 3.14

    print *, p%id
    print *, p%x
end program
