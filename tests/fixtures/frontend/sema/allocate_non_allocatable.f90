program allocate_non_allocatable
  implicit none
  real :: x(10)
  allocate(x(20))
end program allocate_non_allocatable
