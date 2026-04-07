! Type extension (inheritance): child type inherits parent fields.
!
! CHECK: 1.0
! CHECK: 2.0
! CHECK: 5.0
program test_extends
    implicit none
    type :: shape
        real :: x, y
    end type
    type, extends(shape) :: circle
        real :: radius
    end type
    type(circle) :: c
    c%x = 1.0
    c%y = 2.0
    c%radius = 5.0
    print *, c%x
    print *, c%y
    print *, c%radius
end program
